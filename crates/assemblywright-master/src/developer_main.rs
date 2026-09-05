//! Owner-selected supervised developer runner. This is not production execution evidence.
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use uuid::Uuid;

mod developer_process;

#[derive(Debug, thiserror::Error)]
#[error("Could not confirm validation termination; review processes before clearing Emergency Pause: {0}")]
struct UnconfirmedTermination(String);

#[derive(Parser)]
struct Args {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    workspace_root: PathBuf,
    #[arg(long, default_value = "127.0.0.1:7796")]
    bind: std::net::SocketAddr,
    #[arg(long, default_value = "http://127.0.0.1:18080/v1")]
    model_url: String,
    #[arg(long, default_value = "qwen36-local")]
    model: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Edit {
    path: String,
    content: String,
    before: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Feature {
    id: String,
    project: String,
    instruction: String,
    validation: String,
    status: String,
    checkpoint: String,
    message: String,
    edits: Option<Vec<Edit>>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Snapshot {
    revision: u64,
    auto_run: bool,
    emergency_paused: bool,
    queue: Vec<Feature>,
}
struct Database {
    connection: Connection,
    state: Snapshot,
}
struct Engine {
    database: Mutex<Database>,
    running: AtomicBool,
    cancellation: AtomicU8,
    shutdown: AtomicBool,
    root: PathBuf,
    data: PathBuf,
    token: String,
    model_url: String,
    model: String,
}
impl Engine {
    fn change<T>(&self, f: impl FnOnce(&mut Snapshot) -> Result<T>) -> Result<T> {
        self.change_with_commit(f, || {})
    }
    fn change_with_commit<T>(
        &self,
        f: impl FnOnce(&mut Snapshot) -> Result<T>,
        committed: impl FnOnce(),
    ) -> Result<T> {
        let mut db = self
            .database
            .lock()
            .map_err(|_| anyhow!("state lock failed"))?;
        let mut next = db.state.clone();
        let result = f(&mut next)?;
        next.revision += 1;
        let data = serde_json::to_string(&next)?;
        db.connection.execute("INSERT INTO developer_state(id,state) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET state=excluded.state", [data])?;
        db.state = next;
        committed();
        Ok(result)
    }
    fn feature(
        &self,
        id: &str,
        status: &str,
        checkpoint: Option<&str>,
        message: &str,
    ) -> Result<()> {
        self.change(|s| {
            let f = s
                .queue
                .iter_mut()
                .find(|f| f.id == id)
                .context("feature missing")?;
            f.status = status.to_owned();
            if let Some(checkpoint) = checkpoint {
                f.checkpoint = checkpoint.to_owned();
            }
            f.message = message.chars().take(4000).collect();
            Ok(())
        })
    }
    fn snapshot(&self) -> Result<Value> {
        let db = self
            .database
            .lock()
            .map_err(|_| anyhow!("state lock failed"))?;
        let queue: Vec<Value> = db.state.queue.iter().map(|f| json!({
            "id":f.id,"project":f.project,"instruction":f.instruction,"validation":f.validation,
            "status":f.status,"checkpoint":f.checkpoint,"message":f.message,
            "changed_files":f.edits.as_ref().map(|e| e.iter().map(|e| &e.path).collect::<Vec<_>>()).unwrap_or_default()
        })).collect();
        Ok(
            json!({"mode":"supervised_developer","host":std::env::var("COMPUTERNAME").unwrap_or_else(|_|"local".into()),
            "workspace_root":self.root,"revision":db.state.revision,"auto_run":db.state.auto_run,
            "emergency_paused":db.state.emergency_paused || self.cancellation.load(Ordering::SeqCst) == 2,"running":self.running.load(Ordering::SeqCst),"queue":queue}),
        )
    }
    fn cancelled(&self) -> bool {
        self.cancellation.load(Ordering::SeqCst) != 0
    }
    fn start(self: &Arc<Self>) -> Result<()> {
        let db = self
            .database
            .lock()
            .map_err(|_| anyhow!("state lock failed"))?;
        if db.state.emergency_paused || self.cancellation.load(Ordering::SeqCst) == 2 {
            bail!("Clear Emergency Pause before resuming");
        }
        let Some(last_id) = db
            .state
            .queue
            .iter()
            .rev()
            .find(|f| f.status != "succeeded")
            .map(|f| f.id.clone())
        else {
            return Ok(());
        };
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(());
        }
        self.cancellation.store(0, Ordering::SeqCst);
        drop(db);
        let engine = self.clone();
        tokio::spawn(async move {
            if let Err(error) = engine.run_queue(&last_id).await {
                eprintln!("developer run: {error:#}");
            }
            engine.running.store(false, Ordering::SeqCst);
        });
        Ok(())
    }
    async fn run_queue(&self, last_id: &str) -> Result<()> {
        loop {
            let feature =
                self.change(|s| Ok(s.queue.iter().find(|f| f.status != "succeeded").cloned()))?;
            let Some(feature) = feature else {
                break;
            };
            if self.cancelled() {
                break;
            }
            let result = self.run_feature(&feature).await;
            if self.record_failure_or_pause(&feature, result)? {
                break;
            }
            self.feature(
                &feature.id,
                "succeeded",
                Some("validated"),
                "Changes applied and the configured validation command passed.",
            )?;
            if feature.id == last_id
                || !self
                    .database
                    .lock()
                    .map_err(|_| anyhow!("state lock failed"))?
                    .state
                    .auto_run
            {
                break;
            }
        }
        Ok(())
    }
    fn record_failure_or_pause(&self, feature: &Feature, result: Result<()>) -> Result<bool> {
        if let Err(error) = &result {
            if error.downcast_ref::<UnconfirmedTermination>().is_some() {
                self.cancellation.fetch_max(2, Ordering::SeqCst);
                self.change(|s| {
                    s.emergency_paused = true;
                    Ok(())
                })?;
                self.feature(&feature.id, "failed", None, &format!("{error:#}"))?;
                return Ok(true);
            }
        }
        if self.cancelled() {
            self.feature(
                &feature.id,
                "paused",
                None,
                "Stopped. Resume continues from the saved checkpoint; applied files are retained.",
            )?;
            return Ok(true);
        }
        if let Err(error) = result {
            self.feature(&feature.id, "failed", None, &format!("{error:#}"))?;
            return Ok(true);
        }
        Ok(false)
    }
    async fn run_feature(&self, feature: &Feature) -> Result<()> {
        let project = self.root.join(&feature.project);
        fs::create_dir_all(&project)?;
        let project = fs::canonicalize(project)?;
        if !project.starts_with(&self.root) {
            bail!("Project escapes the workspace root");
        }
        self.feature(&feature.id, "running", None, "Preparing the project")?;
        let edits = if let Some(edits) = &feature.edits {
            edits.clone()
        } else {
            self.feature(
                &feature.id,
                "running",
                None,
                "Local model is preparing changes",
            )?;
            let files = project_context(&project)?;
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(900))
                .build()?;
            let request = client.post(format!("{}/chat/completions", self.model_url.trim_end_matches('/'))).json(&json!({
                "model":self.model,"temperature":0.1,"max_tokens":8192,
                "response_format":{"type":"json_object"},
                "chat_template_kwargs":{"enable_thinking":false},
                "messages":[{"role":"system","content":"Implement the requested feature in this project. Return ONLY a JSON object with a files array. Each item has path (relative path) and content (complete UTF-8 file contents). Include only new or changed files. Do not delete files, modify .git, dependencies, or secrets. Preserve existing behavior. Add meaningful tests. No markdown fences. The user supplies the validation command; implement code that really passes it."},
                    {"role":"user","content":format!("Feature: {}\nValidation command: {}\nExisting files: {}",feature.instruction,feature.validation,serde_json::to_string(&files)?)}]
            })).send();
            tokio::pin!(request);
            let response = loop {
                tokio::select! {
                    response = &mut request => break response?,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => if self.cancelled() { bail!("Stopped"); }
                }
            };
            let status = response.status();
            if !status.is_success() {
                bail!("Local model returned HTTP {status}");
            }
            let body = response.json::<Value>();
            tokio::pin!(body);
            let payload = loop {
                tokio::select! {
                    result = &mut body => break result?,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => if self.cancelled() { bail!("Stopped"); }
                }
            };
            let content = payload["choices"][0]["message"]["content"]
                .as_str()
                .context("Model returned no file changes")?;
            if content.len() > 1024 * 1024 {
                bail!("Model change set exceeds 1 MiB");
            }
            let normalized = content
                .trim()
                .strip_prefix("```json")
                .or_else(|| content.trim().strip_prefix("```"))
                .and_then(|s| s.trim().strip_suffix("```"))
                .unwrap_or(content)
                .trim();
            let generated: Value = serde_json::from_str(normalized).with_context(|| format!(
                "Model did not return valid file JSON ({} bytes; finish reason {}); no files were changed", content.len(), payload["choices"][0]["finish_reason"]))?;
            let edits = prepare_edits(&project, &generated, &files)?;
            if self.cancelled() {
                bail!("Stopped");
            }
            self.change(|s| {
                let f = s
                    .queue
                    .iter_mut()
                    .find(|f| f.id == feature.id)
                    .context("feature missing")?;
                f.edits = Some(edits.clone());
                f.checkpoint = "prepared".into();
                Ok(())
            })?;
            edits
        };
        if feature.checkpoint != "applied" && feature.checkpoint != "validated" {
            self.feature(&feature.id, "running", None, "Applying saved file changes")?;
            for edit in &edits {
                if self.cancelled() {
                    bail!("Stopped");
                }
                apply_edit(&project, edit)?;
            }
            self.feature(
                &feature.id,
                "running",
                Some("applied"),
                "Files saved; running validation",
            )?;
        }
        if self.cancelled() {
            bail!("Stopped");
        }
        self.validate_command(feature, &project).await
    }
    async fn validate_command(&self, feature: &Feature, project: &Path) -> Result<()> {
        let log_path = self.data.join(format!("{}.log", feature.id));
        let log = fs::File::create(&log_path)?;
        let mut child =
            developer_process::spawn(&feature.validation, project, log).map_err(|error| {
                if error.is::<developer_process::CleanupUnconfirmed>() {
                    anyhow!(UnconfirmedTermination(format!("{error:#}")))
                } else {
                    error
                }
            })?;
        let started = std::time::Instant::now();
        loop {
            let log_size = match fs::metadata(&log_path) {
                Ok(metadata) => metadata.len(),
                Err(error) => {
                    child
                        .terminate()
                        .await
                        .map_err(|cleanup| UnconfirmedTermination(format!("{cleanup:#}")))?;
                    return Err(error).context("read validation log size after confirmed cleanup");
                }
            };
            if self.cancelled()
                || started.elapsed() > Duration::from_secs(900)
                || log_size > 2 * 1024 * 1024
            {
                child
                    .terminate()
                    .await
                    .map_err(|error| UnconfirmedTermination(format!("{error:#}")))?;
                bail!("Validation stopped or exceeded its time/output limit");
            }
            let observed = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    child
                        .terminate()
                        .await
                        .map_err(|cleanup| UnconfirmedTermination(format!("{cleanup:#}")))?;
                    return Err(error).context("poll validation process after confirmed cleanup");
                }
            };
            if let Some(status) = observed {
                child
                    .terminate()
                    .await
                    .map_err(|error| UnconfirmedTermination(format!("{error:#}")))?;
                if status.success() {
                    return Ok(());
                }
                let bytes = fs::read(&log_path)?;
                let tail = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(3500)..]);
                bail!("Validation failed ({status}):\n{tail}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
fn checked_model_url(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value).context("Invalid local model URL")?;
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
    let local = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if url.scheme() != "http"
        || !local
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("Developer model URL must use local loopback HTTP without credentials, query, or fragment; use SSH forwarding");
    }
    Ok(url.to_string())
}
fn hash(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}
fn checked_path(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.is_empty()
        || relative.contains('\\')
        || relative.contains(':')
        || relative.len() > 240
    {
        bail!("Invalid relative path");
    }
    let path = Path::new(relative);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("File path escapes project");
    }
    let mut full = root.to_path_buf();
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy();
        if name.starts_with('.') || ["node_modules", "target"].contains(&name.as_ref()) {
            bail!("Generated file targets reserved directory");
        }
        full.push(component);
        if full
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            bail!("Symbolic link is not a project file");
        }
        if full.exists() && !fs::canonicalize(&full)?.starts_with(root) {
            bail!("Path leaves project");
        }
    }
    Ok(full)
}
fn prepare_edits(project: &Path, generated: &Value, context: &[Value]) -> Result<Vec<Edit>> {
    let entries = generated["files"]
        .as_array()
        .context("Model response has no files array")?;
    if entries.is_empty() || entries.len() > 40 {
        bail!("Expected 1 to 40 changed files");
    }
    let mut edits = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let path = entry["path"].as_str().context("Missing file path")?;
        let content = entry["content"].as_str().context("Missing file content")?;
        let full = checked_path(project, path)?;
        if !seen.insert(path.to_lowercase()) {
            bail!("Duplicate output file");
        }
        // Bind updates to the exact content the model saw, before its request.
        let observed = context.iter().find(|file| {
            file["path"].as_str().is_some_and(|old| {
                if cfg!(windows) {
                    old.eq_ignore_ascii_case(path)
                } else {
                    old == path
                }
            })
        });
        let before = observed
            .and_then(|file| file["content"].as_str())
            .map(|text| hash(text.as_bytes()));
        let current = if full.exists() {
            Some(hash(&fs::read(&full)?))
        } else {
            None
        };
        if current != before {
            bail!("{path} changed during planning or was absent from the model context; preserve it and reconcile manually");
        }
        edits.push(Edit {
            path: path.into(),
            content: content.into(),
            before,
        });
    }
    Ok(edits)
}
fn apply_edit(root: &Path, edit: &Edit) -> Result<()> {
    let path = checked_path(root, &edit.path)?;
    let current = if path.exists() {
        Some(hash(&fs::read(&path)?))
    } else {
        None
    };
    if current == Some(hash(edit.content.as_bytes())) {
        return Ok(());
    }
    if current != edit.before {
        bail!(
            "{} changed after planning; preserve it and reconcile manually",
            edit.path
        );
    }
    let parent = path.parent().context("No parent directory")?;
    fs::create_dir_all(parent)?;
    let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
    replacement.write_all(edit.content.as_bytes())?;
    replacement.as_file().sync_all()?;
    replacement.persist(&path).map_err(|error| error.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn project_context(root: &Path) -> Result<Vec<Value>> {
    fn visit(
        root: &Path,
        dir: &Path,
        out: &mut Vec<Value>,
        budget: &mut usize,
        depth: usize,
    ) -> Result<()> {
        if depth > 6 {
            return Ok(());
        }
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if *budget >= 64000 || out.len() >= 80 {
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.')
                || ["target", "node_modules", "__pycache__", "venv", "dist"]
                    .contains(&name.as_str())
            {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                visit(root, &entry.path(), out, budget, depth + 1)?;
            } else if kind.is_file() && entry.metadata()?.len() <= 16000 {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    if *budget + content.len() > 64000 {
                        continue;
                    }
                    *budget += content.len();
                    out.push(json!({"path":entry.path().strip_prefix(root)?.to_string_lossy().replace('\\',"/"),"content":content}));
                }
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(root, root, &mut out, &mut 0, 0)?;
    Ok(out)
}
fn authorize(engine: &Engine, headers: &HeaderMap) -> Result<()> {
    if headers.get("authorization").and_then(|h| h.to_str().ok())
        != Some(format!("Bearer {}", engine.token).as_str())
    {
        bail!("Unauthorized");
    }
    Ok(())
}
type Api = (StatusCode, Json<Value>);
fn api(result: Result<Value>) -> Api {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(json!({"error":error.to_string()})),
        ),
    }
}
async fn status(State(engine): State<Arc<Engine>>, headers: HeaderMap) -> Api {
    if authorize(&engine, &headers).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"Unauthorized"})),
        );
    }
    api(engine.snapshot())
}
async fn control(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Api {
    if authorize(&engine, &headers).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"Unauthorized"})),
        );
    }
    let result = (|| -> Result<Value> {
        match request["action"].as_str().context("Missing action")? {
            "enqueue" => {
                let id = request["id"].as_str().context("Missing request ID")?;
                Uuid::parse_str(id)?;
                let project = request["project"].as_str().context("Missing project")?;
                if project.is_empty()
                    || project.len() > 80
                    || !project
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    bail!("Use a simple project folder name");
                }
                let instruction = request["instruction"]
                    .as_str()
                    .context("Missing feature description")?;
                let validation = request["validation"]
                    .as_str()
                    .context("Missing validation command")?;
                if instruction.trim().is_empty()
                    || instruction.len() > 16000
                    || validation.trim().is_empty()
                    || validation.len() > 2000
                {
                    bail!("Feature description and validation command are required");
                }
                engine.change(|s| {
                    if let Some(old) = s.queue.iter().find(|f| f.id == id) {
                        if old.project != project
                            || old.instruction != instruction
                            || old.validation != validation
                        {
                            bail!("Request ID reused with different contents");
                        }
                        return Ok(());
                    }
                    if s.queue.len() >= 100 {
                        bail!("Queue limit is 100");
                    }
                    s.queue.push(Feature {
                        id: id.into(),
                        project: project.into(),
                        instruction: instruction.into(),
                        validation: validation.into(),
                        status: "queued".into(),
                        checkpoint: "not_started".into(),
                        message: "Ready to start".into(),
                        edits: None,
                    });
                    Ok(())
                })?;
            }
            "start" | "resume" => engine.start()?,
            "stop" | "emergency" => {
                let emergency = request["action"] == "emergency";
                engine.change(|s| {
                    engine
                        .cancellation
                        .fetch_max(if emergency { 2 } else { 1 }, Ordering::SeqCst);
                    if emergency {
                        s.emergency_paused = true;
                    }
                    Ok(())
                })?;
            }
            "shutdown" => {
                if engine.running.load(Ordering::SeqCst) {
                    bail!("Stop the active run before shutting down");
                }
                engine.shutdown.store(true, Ordering::SeqCst);
            }
            "clear_emergency" => {
                if engine.running.load(Ordering::SeqCst) {
                    bail!("Wait for the active run to stop");
                }
                engine.change_with_commit(
                    |s| {
                        s.emergency_paused = false;
                        Ok(())
                    },
                    || {
                        engine.cancellation.store(0, Ordering::SeqCst);
                    },
                )?;
            }
            "auto_run" => {
                let enabled = request["enabled"]
                    .as_bool()
                    .context("Missing enabled value")?;
                engine.change(|s| {
                    s.auto_run = enabled;
                    Ok(())
                })?;
            }
            _ => bail!("Unknown action"),
        }
        engine.snapshot()
    })();
    api(result)
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if !args.bind.ip().is_loopback() {
        bail!("Developer runner binds only to loopback; use SSH forwarding");
    }
    let model_url = checked_model_url(&args.model_url)?;
    fs::create_dir_all(&args.data_dir)?;
    fs::create_dir_all(&args.workspace_root)?;
    let instance_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(args.data_dir.join("developer.lock"))?;
    fs2::FileExt::try_lock_exclusive(&instance_lock)
        .context("A developer runner already owns this state directory")?;
    let token_path = args.data_dir.join("developer-token");
    if !token_path.exists() {
        fs::write(&token_path, format!("{}{}", Uuid::new_v4(), Uuid::new_v4()))?;
    }
    let token = fs::read_to_string(&token_path)?;
    let connection = Connection::open(args.data_dir.join("developer.sqlite3"))?;
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS developer_state(id INTEGER PRIMARY KEY CHECK(id=1),state TEXT NOT NULL);")?;
    let mut state: Snapshot =
        match connection.query_row("SELECT state FROM developer_state WHERE id=1", [], |r| {
            r.get::<_, String>(0)
        }) {
            Ok(data) => serde_json::from_str(&data)?,
            Err(rusqlite::Error::QueryReturnedNoRows) => Snapshot {
                auto_run: true,
                ..Default::default()
            },
            Err(e) => return Err(e.into()),
        };
    for feature in &mut state.queue {
        if feature.status == "running" {
            feature.status = "paused".into();
            feature.message =
                "Runner restarted. Review the workspace, then Resume from the saved checkpoint."
                    .into();
        }
    }
    let engine = Arc::new(Engine {
        database: Mutex::new(Database { connection, state }),
        running: AtomicBool::new(false),
        cancellation: AtomicU8::new(0),
        shutdown: AtomicBool::new(false),
        root: fs::canonicalize(args.workspace_root)?,
        data: args.data_dir,
        token,
        model_url,
        model: args.model,
    });
    engine.change(|_| Ok(()))?;
    let app = Router::new()
        .route("/status", get(status))
        .route("/control", post(control))
        .layer(DefaultBodyLimit::max(32768))
        .with_state(engine.clone());
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    println!(
        "Assemblywright supervised developer runner ready at {}",
        args.bind
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            while !engine.shutdown.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn engine() -> (tempfile::TempDir, Arc<Engine>) {
        let dir = tempfile::tempdir().unwrap();
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE developer_state(id INTEGER PRIMARY KEY,state TEXT NOT NULL);",
            )
            .unwrap();
        let engine = Arc::new(Engine {
            database: Mutex::new(Database {
                connection,
                state: Snapshot {
                    auto_run: true,
                    ..Default::default()
                },
            }),
            running: AtomicBool::new(false),
            cancellation: AtomicU8::new(0),
            shutdown: AtomicBool::new(false),
            root: fs::canonicalize(dir.path()).unwrap(),
            data: dir.path().into(),
            token: "test-token".into(),
            model_url: "http://127.0.0.1:1".into(),
            model: "fixture".into(),
        });
        (dir, engine)
    }
    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test-token".parse().unwrap());
        headers
    }
    fn enqueue() -> Value {
        json!({"action":"enqueue","id":Uuid::new_v4(),"project":"project","instruction":"Write tests","validation":"test-command"})
    }
    #[test]
    fn model_endpoint_cannot_disclose_project_context_off_host() {
        for value in [
            "http://127.0.0.1:8080/v1",
            "http://localhost:18080/v1",
            "http://[::1]:8080/v1",
        ] {
            assert!(checked_model_url(value).is_ok(), "{value}");
        }
        for value in [
            "https://example.com/v1",
            "http://192.0.2.1/v1",
            "file:///tmp/model",
            "http://user:secret@localhost/v1",
            "http://localhost/v1?key=value",
            "http://localhost/v1#fragment",
            "not a URL",
        ] {
            assert!(checked_model_url(value).is_err(), "{value}");
        }
    }
    #[tokio::test]
    async fn failed_emergency_commit_keeps_volatile_latch_until_successful_clear() {
        let (_dir, engine) = engine();
        assert_eq!(
            control(State(engine.clone()), headers(), Json(enqueue()))
                .await
                .0,
            StatusCode::OK
        );
        engine.database.lock().unwrap().connection.execute_batch("CREATE TRIGGER reject_write BEFORE UPDATE ON developer_state BEGIN SELECT RAISE(ABORT,'fixture write failure'); END;").unwrap();
        assert_eq!(
            control(
                State(engine.clone()),
                headers(),
                Json(json!({"action":"emergency"}))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(engine.snapshot().unwrap()["emergency_paused"], true);
        assert!(engine.start().is_err());
        assert_eq!(
            control(
                State(engine.clone()),
                headers(),
                Json(json!({"action":"clear_emergency"}))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(engine.snapshot().unwrap()["emergency_paused"], true);
        assert!(engine.start().is_err());
        engine
            .database
            .lock()
            .unwrap()
            .connection
            .execute_batch("DROP TRIGGER reject_write;")
            .unwrap();
        assert_eq!(
            control(
                State(engine.clone()),
                headers(),
                Json(json!({"action":"clear_emergency"}))
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(engine.snapshot().unwrap()["emergency_paused"], false);
        assert!(!engine.running.load(Ordering::SeqCst));
    }
    #[tokio::test]
    async fn uncertain_termination_dominates_cancel_and_latches_emergency() {
        let (_dir, engine) = engine();
        assert_eq!(
            control(State(engine.clone()), headers(), Json(enqueue()))
                .await
                .0,
            StatusCode::OK
        );
        let feature = engine.database.lock().unwrap().state.queue[0].clone();
        engine.cancellation.store(1, Ordering::SeqCst);
        let error = UnconfirmedTermination("fixture process query failure".into());
        assert!(engine
            .record_failure_or_pause(&feature, Err(error.into()))
            .unwrap());
        let observed = engine.snapshot().unwrap();
        assert_eq!(observed["emergency_paused"], true);
        assert_eq!(observed["queue"][0]["status"], "failed");
        assert!(observed["queue"][0]["message"]
            .as_str()
            .unwrap()
            .contains("Could not confirm"));
        assert!(engine.start().is_err());
    }
    #[test]
    fn failed_database_write_cannot_publish_memory_only_state() {
        let (_dir, engine) = engine();
        engine
            .change(|s| {
                s.auto_run = false;
                Ok(())
            })
            .unwrap();
        engine.database.lock().unwrap().connection.execute_batch("CREATE TRIGGER reject_write BEFORE UPDATE ON developer_state BEGIN SELECT RAISE(ABORT,'fixture write failure'); END;").unwrap();
        assert!(engine
            .change(|s| {
                s.auto_run = true;
                Ok(())
            })
            .is_err());
        let snapshot = engine.snapshot().unwrap();
        assert_eq!(snapshot["auto_run"], false);
        assert_eq!(snapshot["revision"], 1);
    }
    #[tokio::test]
    async fn enqueue_rejects_invalid_and_oversized_fields_without_mutation() {
        let (_dir, engine) = engine();
        for (key, value) in [
            ("id", "invalid".into()),
            ("project", "../escape".into()),
            ("project", "x".repeat(81)),
            ("instruction", " ".into()),
            ("instruction", "x".repeat(16001)),
            ("validation", "".into()),
            ("validation", "x".repeat(2001)),
        ] {
            let mut body = enqueue();
            body[key] = Value::String(value);
            let (status, _) = control(State(engine.clone()), headers(), Json(body)).await;
            assert_eq!(status, StatusCode::CONFLICT, "{key}");
            assert_eq!(engine.snapshot().unwrap()["queue"], json!([]));
            assert_eq!(engine.snapshot().unwrap()["revision"], 0);
        }
    }
    #[tokio::test]
    async fn queue_capacity_allows_exact_replay_but_not_new_work() {
        let (_dir, engine) = engine();
        let mut last = enqueue();
        for _ in 0..100 {
            last = enqueue();
            assert_eq!(
                control(State(engine.clone()), headers(), Json(last.clone()))
                    .await
                    .0,
                StatusCode::OK
            );
        }
        assert_eq!(
            control(State(engine.clone()), headers(), Json(last))
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            control(State(engine.clone()), headers(), Json(enqueue()))
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            engine.snapshot().unwrap()["queue"]
                .as_array()
                .unwrap()
                .len(),
            100
        );
    }
    #[tokio::test]
    async fn unauthorized_control_cannot_latch_pause_or_enqueue() {
        let (_dir, engine) = engine();
        for body in [enqueue(), json!({"action":"emergency"})] {
            assert_eq!(
                control(State(engine.clone()), HeaderMap::new(), Json(body))
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(engine.snapshot().unwrap()["revision"], 0);
        assert!(!engine.cancelled());
    }
    #[tokio::test]
    async fn emergency_requires_clear_and_never_starts_on_clear() {
        let (_dir, engine) = engine();
        assert_eq!(
            control(
                State(engine.clone()),
                headers(),
                Json(json!({"action":"emergency"}))
            )
            .await
            .0,
            StatusCode::OK
        );
        assert!(engine.start().is_err());
        assert_eq!(
            control(
                State(engine.clone()),
                headers(),
                Json(json!({"action":"clear_emergency"}))
            )
            .await
            .0,
            StatusCode::OK
        );
        assert!(!engine.running.load(Ordering::SeqCst));
        assert_eq!(engine.snapshot().unwrap()["emergency_paused"], false);
    }
    #[test]
    fn planning_preserves_owner_edits_and_unobserved_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let generated = json!({"files":[{"path":"example.txt","content":"model edit"}]});
        let context = vec![json!({"path":"example.txt","content":"original"})];
        fs::write(root.join("example.txt"), "owner edit").unwrap();
        assert!(prepare_edits(&root, &generated, &context).is_err());
        assert!(prepare_edits(&root, &generated, &[]).is_err());
        assert_eq!(
            fs::read_to_string(root.join("example.txt")).unwrap(),
            "owner edit"
        );
        fs::write(root.join("example.txt"), "original").unwrap();
        let edits = prepare_edits(&root, &generated, &context).unwrap();
        apply_edit(&root, &edits[0]).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("example.txt")).unwrap(),
            "model edit"
        );
    }
    #[test]
    fn generated_change_set_checks_count_shape_and_duplicates_before_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for body in [
            json!({}),
            json!({"files":[]}),
            json!({"files":[{"path":"x"}]}),
            json!({"files":[{"path":"x","content":"a"},{"path":"X","content":"b"}]}),
            json!({"files":(0..41).map(|n| json!({"path":format!("file{n}"),"content":"x"})).collect::<Vec<_>>()}),
        ] {
            assert!(prepare_edits(&root, &body, &[]).is_err());
        }
        let maximum = json!({"files":(0..40).map(|n| json!({"path":format!("file{n}"),"content":"x"})).collect::<Vec<_>>()});
        assert_eq!(prepare_edits(&root, &maximum, &[]).unwrap().len(), 40);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }
    #[test]
    fn project_context_skips_hidden_dependencies_binary_and_over_budget_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/ignored"), "ignored").unwrap();
        fs::write(root.join(".secret"), "ignored").unwrap();
        fs::write(root.join("binary"), [0xff]).unwrap();
        for n in 0..8 {
            fs::write(root.join(format!("file{n}")), "x".repeat(15000)).unwrap();
        }
        let context = project_context(&root).unwrap();
        assert_eq!(context.len(), 4);
        assert!(context
            .iter()
            .all(|v| v["path"].as_str().unwrap().starts_with("file")));
        assert_eq!(
            context
                .iter()
                .map(|v| v["content"].as_str().unwrap().len())
                .sum::<usize>(),
            60000
        );
    }
    #[test]
    fn prepared_edits_resume_once_and_preserve_owner_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let edit = Edit {
            path: "src/example.txt".into(),
            content: "implemented".into(),
            before: None,
        };
        apply_edit(&root, &edit).unwrap();
        apply_edit(&root, &edit).unwrap();
        fs::write(root.join(&edit.path), "owner edit").unwrap();
        assert!(apply_edit(&root, &edit).is_err());
        assert_eq!(
            fs::read_to_string(root.join(&edit.path)).unwrap(),
            "owner edit"
        );
    }
    #[test]
    fn generated_paths_remain_in_selected_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for path in [
            "../outside",
            "/tmp/outside",
            ".git/config",
            "a/../../outside",
            "C:/outside",
            "a\\b",
        ] {
            assert!(checked_path(&root, path).is_err(), "{path}");
        }
        assert!(checked_path(&root, "src/main.py").is_ok());
    }
}

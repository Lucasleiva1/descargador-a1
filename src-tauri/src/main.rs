use regex::Regex;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, Notify},
};
use url::Url;

const SCANNER_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) DescargadorA1/0.1 authorized-link-scanner";
const MAX_FOLLOW_LINKS: usize = 12;
const FOLLOW_DELAY_MS: u64 = 450;

#[derive(Default)]
struct DownloadState {
    active: Arc<AtomicBool>,
    jobs: Arc<Mutex<HashMap<String, Arc<JobCancellation>>>>,
}

#[derive(Default)]
struct JobCancellation {
    requested: AtomicBool,
    notify: Notify,
}

impl JobCancellation {
    fn cancel(&self) {
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Serialize)]
struct ToolProbe {
    available: bool,
    path: Option<String>,
    version: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct ToolState {
    yt_dlp: ToolProbe,
    ffmpeg: ToolProbe,
    javascript: ToolProbe,
    default_output_dir: Option<String>,
}

#[derive(Debug, Serialize)]
struct ProbeEntry {
    id: Option<String>,
    title: Option<String>,
    url: String,
    webpage_url: Option<String>,
    duration: Option<String>,
    kind: Option<String>,
    source: Option<String>,
    resolutions: Vec<u32>,
}

#[derive(Debug, Serialize)]
struct ProbeResult {
    source_url: String,
    title: Option<String>,
    entries: Vec<ProbeEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct DownloadJob {
    id: String,
    title: Option<String>,
    url: String,
    #[serde(rename = "maxHeight")]
    max_height: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
struct DownloadUpdate {
    id: String,
    status: String,
    progress: Option<f64>,
    speed: Option<String>,
    eta: Option<String>,
    file: Option<String>,
    message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct BatchState {
    active: bool,
    message: Option<String>,
}

#[tauri::command]
async fn check_tools(app: AppHandle) -> ToolState {
    let yt_dlp = probe_command(locate_yt_dlp(&app).ok(), &["--version"]).await;
    let ffmpeg = probe_command(locate_ffmpeg(), &["-version"]).await;
    let javascript = probe_command(locate_node(), &["--version"]).await;

    ToolState {
        yt_dlp,
        ffmpeg,
        javascript,
        default_output_dir: default_output_dir(),
    }
}

#[tauri::command]
async fn probe_url(
    app: AppHandle,
    url: String,
    browser: Option<String>,
) -> Result<ProbeResult, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("Pegá una URL válida.".to_string());
    }

    let yt_dlp = locate_yt_dlp(&app)?;
    let mut args = vec![
        "--dump-single-json".to_string(),
        "--flat-playlist".to_string(),
        "--no-color".to_string(),
        "--ignore-config".to_string(),
        "--socket-timeout".to_string(),
        "30".to_string(),
        "--retries".to_string(),
        "3".to_string(),
    ];
    append_runtime_args(&mut args);
    append_browser_args(&mut args, browser.as_deref())?;
    args.push(url.clone());

    let output = Command::new(&yt_dlp)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| format!("No pude ejecutar yt-dlp: {error}"))?;

    if !output.status.success() {
        return Err(clean_process_error(
            &output.stderr,
            "No pude extraer esa URL.",
        ));
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("yt-dlp devolvió JSON inválido: {error}"))?;

    Ok(parse_probe_result(&url, value))
}

#[tauri::command]
async fn scan_page(url: String, referer: Option<String>) -> Result<ProbeResult, String> {
    let parsed_url = Url::parse(url.trim())
        .map_err(|error| format!("No pude interpretar esa URL para escanear la pagina: {error}"))?;

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(35))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(SCANNER_USER_AGENT)
        .build()
        .map_err(|error| format!("No pude preparar el escaner: {error}"))?;

    let (final_url, html) = fetch_html(
        &client,
        &parsed_url,
        referer
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    )
    .await?;

    let title = extract_page_title(&html);
    let mut entries = extract_page_candidates(&final_url, &html);
    let mut seen = entries
        .iter()
        .map(|entry| entry.url.clone())
        .collect::<HashSet<_>>();

    if let Some(rendered_html) = render_page(&final_url).await {
        for entry in extract_page_candidates(&final_url, &rendered_html) {
            if seen.insert(entry.url.clone()) {
                entries.push(entry);
            }
        }
    }

    let follow_targets = entries
        .iter()
        .filter_map(|entry| follow_target(entry, &final_url))
        .take(MAX_FOLLOW_LINKS)
        .collect::<Vec<_>>();

    for target in follow_targets {
        tokio::time::sleep(Duration::from_millis(FOLLOW_DELAY_MS)).await;

        let Ok((detail_url, detail_html)) =
            fetch_html(&client, &target, Some(final_url.as_str())).await
        else {
            continue;
        };

        for entry in extract_page_candidates(&detail_url, &detail_html) {
            if seen.insert(entry.url.clone()) {
                entries.push(entry);
            }
        }
    }

    if entries.is_empty() {
        return Err(
            "No encontre links descargables visibles. Si la pagina los crea solo despues de ejecutar JavaScript, haria falta un escaneo con navegador para paginas propias o autorizadas.".to_string(),
        );
    }

    Ok(ProbeResult {
        source_url: final_url.to_string(),
        title,
        entries,
    })
}

async fn render_page(url: &Url) -> Option<String> {
    let edge = locate_edge()?;
    let profile = std::env::temp_dir().join(format!(
        "descargador-a1-edge-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis()
    ));

    let mut command = Command::new(edge);
    command
        .args([
            "--headless=new",
            "--disable-gpu",
            "--disable-extensions",
            "--disable-background-networking",
            "--dump-dom",
            "--virtual-time-budget=12000",
        ])
        .arg(format!("--user-data-dir={}", profile.to_string_lossy()))
        .arg(url.as_str())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(Duration::from_secs(35), command.output()).await;
    let _ = fs::remove_dir_all(profile);
    let output = output.ok()?.ok()?;

    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
        .filter(|html| !html.trim().is_empty())
}

async fn fetch_html(
    client: &reqwest::Client,
    url: &Url,
    referer: Option<&str>,
) -> Result<(Url, String), String> {
    let mut request = client
        .get(url.clone())
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "es-AR,es;q=0.9,en;q=0.7");

    if let Some(referer) = referer {
        request = request.header(reqwest::header::REFERER, referer);
    }

    let response = request
        .send()
        .await
        .map_err(|error| format!("No pude abrir la pagina para escanearla: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("La pagina respondio con estado HTTP {status}."));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    if !content_type.is_empty()
        && !content_type.contains("text/html")
        && !content_type.contains("application/xhtml")
    {
        return Err(format!(
            "El destino no parece una pagina HTML ({content_type})."
        ));
    }

    let final_url = response.url().clone();
    let html = response
        .text()
        .await
        .map_err(|error| format!("No pude leer el HTML de la pagina: {error}"))?;

    Ok((final_url, html))
}

#[tauri::command]
async fn start_download(
    app: AppHandle,
    state: State<'_, DownloadState>,
    jobs: Vec<DownloadJob>,
    output_dir: Option<String>,
    referer: Option<String>,
    browser: Option<String>,
) -> Result<(), String> {
    if jobs.is_empty() {
        return Err("La cola está vacía.".to_string());
    }

    if state.active.swap(true, Ordering::SeqCst) {
        return Err("Ya hay una descarga activa.".to_string());
    }

    let active = state.active.clone();
    let yt_dlp = match locate_yt_dlp(&app) {
        Ok(path) => path,
        Err(error) => {
            active.store(false, Ordering::SeqCst);
            return Err(error);
        }
    };

    let output_dir = normalize_output_dir(output_dir)?;
    if let Some(path) = &output_dir {
        fs::create_dir_all(path)
            .map_err(|error| format!("No pude crear la carpeta de salida: {error}"))?;
    }

    let job_controls = state.jobs.clone();
    {
        let mut controls = job_controls.lock().await;
        for job in &jobs {
            controls.insert(job.id.clone(), Arc::new(JobCancellation::default()));
        }
    }

    tauri::async_runtime::spawn(async move {
        let _ = app.emit(
            "download://batch-state",
            BatchState {
                active: true,
                message: Some("Cola iniciada.".to_string()),
            },
        );

        for job in jobs {
            let cancellation = {
                let controls = job_controls.lock().await;
                controls.get(&job.id).cloned()
            };

            if let Some(cancellation) = cancellation {
                if cancellation.is_cancelled() {
                    emit_cancelled(&app, &job.id);
                } else {
                    run_download_job(
                        &app,
                        &yt_dlp,
                        &job,
                        output_dir.as_ref(),
                        referer.as_deref(),
                        browser.as_deref(),
                        cancellation,
                    )
                    .await;
                }
            }

            job_controls.lock().await.remove(&job.id);
        }

        active.store(false, Ordering::SeqCst);
        let _ = app.emit(
            "download://batch-state",
            BatchState {
                active: false,
                message: Some("Cola finalizada.".to_string()),
            },
        );
    });

    Ok(())
}

#[tauri::command]
async fn cancel_download(state: State<'_, DownloadState>, id: String) -> Result<bool, String> {
    let cancellation = {
        let controls = state.jobs.lock().await;
        controls.get(&id).cloned()
    };

    if let Some(cancellation) = cancellation {
        cancellation.cancel();
        Ok(true)
    } else {
        Ok(false)
    }
}

async fn run_download_job(
    app: &AppHandle,
    yt_dlp: &PathBuf,
    job: &DownloadJob,
    output_dir: Option<&PathBuf>,
    referer: Option<&str>,
    browser: Option<&str>,
    cancellation: Arc<JobCancellation>,
) {
    emit_update(
        app,
        DownloadUpdate {
            id: job.id.clone(),
            status: "extracting".to_string(),
            progress: Some(0.0),
            speed: None,
            eta: None,
            file: None,
            message: job.title.clone().or_else(|| Some(job.url.clone())),
        },
    );

    let mut args = vec![
        "--newline".to_string(),
        "--no-color".to_string(),
        "--ignore-config".to_string(),
        "--windows-filenames".to_string(),
        "--no-playlist".to_string(),
        "--continue".to_string(),
        "--part".to_string(),
        "--retries".to_string(),
        "10".to_string(),
        "--fragment-retries".to_string(),
        "10".to_string(),
        "--file-access-retries".to_string(),
        "10".to_string(),
        "--retry-sleep".to_string(),
        "fragment:exp=1:20".to_string(),
        "--socket-timeout".to_string(),
        "30".to_string(),
        "--concurrent-fragments".to_string(),
        "4".to_string(),
        "--format".to_string(),
        format_selector(job.max_height),
        "--merge-output-format".to_string(),
        "mp4".to_string(),
        "--no-overwrites".to_string(),
        "--print".to_string(),
        "after_move:[download] Final file: %(filepath)s".to_string(),
        "-o".to_string(),
        "%(title).200B [%(id)s].%(ext)s".to_string(),
    ];

    append_runtime_args(&mut args);
    append_ffmpeg_args(&mut args);
    if let Err(error) = append_browser_args(&mut args, browser) {
        emit_update(
            app,
            DownloadUpdate {
                id: job.id.clone(),
                status: "failed".to_string(),
                progress: None,
                speed: None,
                eta: None,
                file: None,
                message: Some(error),
            },
        );
        return;
    }

    if let Some(path) = output_dir {
        args.push("-P".to_string());
        args.push(path.to_string_lossy().to_string());
    }

    if let Some(referer) = referer {
        if !referer.trim().is_empty() {
            args.push("--referer".to_string());
            args.push(referer.trim().to_string());
        }
    }

    args.push(job.url.clone());

    let mut child = match Command::new(yt_dlp)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            emit_update(
                app,
                DownloadUpdate {
                    id: job.id.clone(),
                    status: "failed".to_string(),
                    progress: None,
                    speed: None,
                    eta: None,
                    file: None,
                    message: Some(format!("No pude iniciar yt-dlp: {error}")),
                },
            );
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_app = app.clone();
    let stderr_app = app.clone();
    let stdout_job = job.id.clone();
    let stderr_job = job.id.clone();

    let stdout_task = stdout.map(|stream| {
        tokio::spawn(async move {
            read_process_stream(stdout_app, stdout_job, stream).await;
        })
    });

    let stderr_task = stderr.map(|stream| {
        tokio::spawn(async move { read_process_stream(stderr_app, stderr_job, stream).await })
    });

    let result = if cancellation.is_cancelled() {
        terminate_child(&mut child).await;
        None
    } else {
        tokio::select! {
            result = child.wait() => Some(result),
            _ = cancellation.notify.notified() => {
                terminate_child(&mut child).await;
                None
            }
        }
    };

    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    let stderr_message = if let Some(task) = stderr_task {
        task.await.ok().flatten()
    } else {
        None
    };

    match result {
        None => emit_cancelled(app, &job.id),
        Some(Ok(status)) if status.success() => emit_update(
            app,
            DownloadUpdate {
                id: job.id.clone(),
                status: "completed".to_string(),
                progress: Some(100.0),
                speed: None,
                eta: None,
                file: None,
                message: Some("Descarga completa.".to_string()),
            },
        ),
        Some(Ok(status)) => emit_update(
            app,
            DownloadUpdate {
                id: job.id.clone(),
                status: "failed".to_string(),
                progress: None,
                speed: None,
                eta: None,
                file: None,
                message: Some(
                    stderr_message.unwrap_or_else(|| format!("yt-dlp terminó con código {status}")),
                ),
            },
        ),
        Some(Err(error)) => emit_update(
            app,
            DownloadUpdate {
                id: job.id.clone(),
                status: "failed".to_string(),
                progress: None,
                speed: None,
                eta: None,
                file: None,
                message: Some(format!("Error esperando a yt-dlp: {error}")),
            },
        ),
    }
}

async fn terminate_child(child: &mut Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn emit_cancelled(app: &AppHandle, id: &str) {
    emit_update(
        app,
        DownloadUpdate {
            id: id.to_string(),
            status: "cancelled".to_string(),
            progress: None,
            speed: None,
            eta: None,
            file: None,
            message: Some("Descarga detenida.".to_string()),
        },
    );
}

async fn read_process_stream<R>(app: AppHandle, job_id: String, stream: R) -> Option<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stream).lines();
    let mut last_error = None;
    let mut last_warning = None;

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        let lowercase = line.to_ascii_lowercase();
        if line.starts_with("ERROR:") || lowercase.contains("error:") {
            last_error = Some(line.to_string());
        } else if line.starts_with("WARNING:") {
            last_warning = Some(line.to_string());
        }

        if let Some(update) = parse_download_line(&job_id, line) {
            emit_update(&app, update);
        }
    }

    last_error.or(last_warning)
}

fn parse_download_line(job_id: &str, line: &str) -> Option<DownloadUpdate> {
    if line.is_empty() {
        return None;
    }

    if let Some(file) = line.strip_prefix("[download] Final file: ") {
        return Some(DownloadUpdate {
            id: job_id.to_string(),
            status: "completed".to_string(),
            progress: Some(100.0),
            speed: None,
            eta: None,
            file: Some(file.trim().to_string()),
            message: Some("Archivo listo.".to_string()),
        });
    }

    if let Some(file) = line.strip_prefix("[download] Destination: ") {
        return Some(DownloadUpdate {
            id: job_id.to_string(),
            status: "downloading".to_string(),
            progress: None,
            speed: None,
            eta: None,
            file: Some(file.trim().to_string()),
            message: Some("Archivo destino detectado.".to_string()),
        });
    }

    if let Some(file) = line.strip_prefix("[Merger] Merging formats into ") {
        return Some(DownloadUpdate {
            id: job_id.to_string(),
            status: "processing".to_string(),
            progress: None,
            speed: None,
            eta: None,
            file: Some(file.trim_matches('"').to_string()),
            message: Some("Uniendo audio y video.".to_string()),
        });
    }

    if line.contains("[ExtractAudio]") || line.contains("[VideoRemuxer]") {
        return Some(DownloadUpdate {
            id: job_id.to_string(),
            status: "processing".to_string(),
            progress: None,
            speed: None,
            eta: None,
            file: None,
            message: Some(line.to_string()),
        });
    }

    if line.starts_with("[Merger]")
        || line.starts_with("[VideoConvertor]")
        || line.starts_with("[Fixup")
        || line.starts_with("Deleting original file")
    {
        return Some(DownloadUpdate {
            id: job_id.to_string(),
            status: "processing".to_string(),
            progress: None,
            speed: None,
            eta: None,
            file: None,
            message: Some(line.to_string()),
        });
    }

    if line.starts_with("WARNING:") {
        return Some(DownloadUpdate {
            id: job_id.to_string(),
            status: "extracting".to_string(),
            progress: None,
            speed: None,
            eta: None,
            file: None,
            message: Some(line.to_string()),
        });
    }

    if !line.contains("[download]") {
        if line.starts_with("ERROR:") {
            return Some(DownloadUpdate {
                id: job_id.to_string(),
                status: "failed".to_string(),
                progress: None,
                speed: None,
                eta: None,
                file: None,
                message: Some(line.to_string()),
            });
        }

        if line.starts_with('[') {
            return Some(DownloadUpdate {
                id: job_id.to_string(),
                status: "extracting".to_string(),
                progress: None,
                speed: None,
                eta: None,
                file: None,
                message: Some(line.to_string()),
            });
        }

        return None;
    }

    if line.contains("has already been downloaded") {
        return Some(DownloadUpdate {
            id: job_id.to_string(),
            status: "completed".to_string(),
            progress: Some(100.0),
            speed: None,
            eta: None,
            file: None,
            message: Some("El archivo ya existía.".to_string()),
        });
    }

    if line.starts_with("[download] Downloading")
        || line.starts_with("[download] Got error")
        || line.starts_with("[download] Unable")
    {
        return Some(DownloadUpdate {
            id: job_id.to_string(),
            status: "downloading".to_string(),
            progress: None,
            speed: None,
            eta: None,
            file: None,
            message: Some(line.to_string()),
        });
    }

    let percent = parse_percent(line);
    let (speed, eta) = parse_speed_eta(line);

    if percent.is_some() || speed.is_some() || eta.is_some() {
        return Some(DownloadUpdate {
            id: job_id.to_string(),
            status: "downloading".to_string(),
            progress: percent,
            speed,
            eta,
            file: None,
            message: None,
        });
    }

    None
}

fn parse_percent(line: &str) -> Option<f64> {
    let pct_index = line.find('%')?;
    let before = &line[..pct_index];
    before
        .split_whitespace()
        .last()
        .and_then(|token| token.parse::<f64>().ok())
}

fn parse_speed_eta(line: &str) -> (Option<String>, Option<String>) {
    let Some(at_index) = line.find(" at ") else {
        return (None, None);
    };
    let rest = &line[at_index + 4..];
    let Some(eta_index) = rest.find(" ETA ") else {
        return (Some(rest.trim().to_string()), None);
    };

    let speed = rest[..eta_index].trim();
    let eta = rest[eta_index + 5..].trim();
    (
        (!speed.is_empty()).then(|| speed.to_string()),
        (!eta.is_empty()).then(|| eta.to_string()),
    )
}

fn emit_update(app: &AppHandle, update: DownloadUpdate) {
    let _ = app.emit("download://job-update", update);
}

async fn probe_command(path: Option<PathBuf>, version_args: &[&str]) -> ToolProbe {
    let Some(path) = path else {
        return ToolProbe {
            available: false,
            path: None,
            version: None,
            message: Some("No encontrado.".to_string()),
        };
    };

    let output = Command::new(&path).args(version_args).output().await;
    match output {
        Ok(output) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout);
            let version = raw.lines().next().map(|line| {
                line.split_whitespace()
                    .next()
                    .unwrap_or(line)
                    .trim()
                    .to_string()
            });

            ToolProbe {
                available: true,
                path: Some(path.to_string_lossy().to_string()),
                version,
                message: None,
            }
        }
        Ok(output) => ToolProbe {
            available: false,
            path: Some(path.to_string_lossy().to_string()),
            version: None,
            message: Some(clean_process_error(
                &output.stderr,
                "No pude consultar versión.",
            )),
        },
        Err(error) => ToolProbe {
            available: false,
            path: Some(path.to_string_lossy().to_string()),
            version: None,
            message: Some(error.to_string()),
        },
    }
}

fn parse_probe_result(source_url: &str, value: Value) -> ProbeResult {
    let title = value_string(&value, "title");
    let mut entries = Vec::new();

    if let Some(array) = value.get("entries").and_then(|entries| entries.as_array()) {
        for entry in array {
            if let Some(parsed) = parse_entry(source_url, entry) {
                entries.push(parsed);
            }
        }
    }

    if entries.is_empty() {
        let url = value_string(&value, "webpage_url")
            .or_else(|| value_string(&value, "url"))
            .unwrap_or_else(|| source_url.to_string());

        entries.push(ProbeEntry {
            id: value_string(&value, "id"),
            title: title.clone(),
            url: url.clone(),
            webpage_url: Some(url),
            duration: duration_string(&value),
            kind: Some("yt-dlp".to_string()),
            source: Some("extractor".to_string()),
            resolutions: extract_resolutions(&value),
        });
    }

    ProbeResult {
        source_url: source_url.to_string(),
        title,
        entries,
    }
}

fn parse_entry(source_url: &str, value: &Value) -> Option<ProbeEntry> {
    let url = value_string(value, "webpage_url")
        .or_else(|| value_string(value, "url"))
        .unwrap_or_else(|| source_url.to_string());

    if url.trim().is_empty() {
        return None;
    }

    Some(ProbeEntry {
        id: value_string(value, "id"),
        title: value_string(value, "title"),
        webpage_url: value_string(value, "webpage_url"),
        url,
        duration: duration_string(value),
        kind: Some("yt-dlp".to_string()),
        source: Some("extractor".to_string()),
        resolutions: extract_resolutions(value),
    })
}

fn extract_resolutions(value: &Value) -> Vec<u32> {
    let mut heights = value
        .get("formats")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|format| {
            format
                .get("vcodec")
                .and_then(Value::as_str)
                .is_some_and(|codec| codec != "none")
        })
        .filter_map(|format| format.get("height").and_then(Value::as_u64))
        .filter_map(|height| u32::try_from(height).ok())
        .filter(|height| *height > 0)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    heights.reverse();
    heights
}

fn extract_page_title(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("title").ok()?;
    document
        .select(&selector)
        .next()
        .map(|node| clean_label(&node.text().collect::<Vec<_>>().join(" ")))
        .filter(|text| !text.is_empty())
}

fn extract_page_candidates(base_url: &Url, html: &str) -> Vec<ProbeEntry> {
    let document = Html::parse_document(html);
    let mut seen = HashSet::new();
    let mut scored = Vec::new();

    let link_attrs = [
        "href",
        "src",
        "action",
        "data-url",
        "data-href",
        "data-link",
        "data-src",
        "data-file",
        "data-download",
        "data-target-url",
        "data-redirect",
        "content",
    ];
    let Ok(selector) = Selector::parse(
        "[href], [src], form[action], [data-url], [data-href], [data-link], [data-src], [data-file], [data-download], [data-target-url], [data-redirect], [onclick], meta[content]",
    ) else {
        return Vec::new();
    };

    for element in document.select(&selector) {
        let source_tag = element.value().name();
        let label = element_label(&element, source_tag);

        for attr in link_attrs {
            let Some(raw_url) = element.value().attr(attr) else {
                continue;
            };

            if attr == "content" {
                if let Some(target) = meta_refresh_target(raw_url) {
                    push_candidate(
                        base_url,
                        &target,
                        &label,
                        source_tag,
                        &mut seen,
                        &mut scored,
                    );
                }
                continue;
            }

            push_candidate(
                base_url,
                raw_url,
                &label,
                source_tag,
                &mut seen,
                &mut scored,
            );
        }

        if let Some(onclick) = element.value().attr("onclick") {
            for raw_url in harvest_navigation_targets(onclick) {
                push_candidate(
                    base_url,
                    &raw_url,
                    &label,
                    source_tag,
                    &mut seen,
                    &mut scored,
                );
            }
        }
    }

    for raw_url in harvest_urls_from_text(html) {
        push_candidate(base_url, &raw_url, "", "html", &mut seen, &mut scored);
    }

    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.url.cmp(&right.1.url))
    });
    scored.into_iter().map(|(_, entry)| entry).collect()
}

fn follow_target(entry: &ProbeEntry, source_url: &Url) -> Option<Url> {
    let target = entry.webpage_url.as_ref().unwrap_or(&entry.url);
    let parsed = Url::parse(target).ok()?;

    if parsed.as_str() == source_url.as_str() || is_direct_file_url(&parsed) {
        return None;
    }

    let title = entry.title.as_deref().unwrap_or("").to_lowercase();
    let kind = entry.kind.as_deref().unwrap_or("").to_lowercase();
    let raw = parsed.as_str().to_lowercase();

    if contains_any(
        &format!("{title} {kind} {raw}"),
        &[
            "boton",
            "descarga",
            "descargar",
            "download",
            "bajar",
            "servidor",
            "server",
            "opcion",
            "opci",
            "mirror",
            "reproductor",
            "player",
            "embed",
            "stream",
            "watch",
        ],
    ) {
        return Some(parsed);
    }

    None
}

fn push_candidate(
    base_url: &Url,
    raw_url: &str,
    label: &str,
    source_tag: &str,
    seen: &mut HashSet<String>,
    scored: &mut Vec<(i32, ProbeEntry)>,
) {
    let Some(resolved) = resolve_url(base_url, raw_url) else {
        return;
    };

    let url = resolved.to_string();
    if !seen.insert(url.clone()) {
        return;
    }

    let Some((kind, score)) = classify_candidate(&resolved, source_tag, label) else {
        return;
    };

    let title = if label.is_empty() {
        compact_url_title(&resolved)
    } else {
        clean_label(label)
    };

    scored.push((
        score,
        ProbeEntry {
            id: None,
            title: Some(title),
            url: url.clone(),
            webpage_url: Some(url),
            duration: None,
            kind: Some(kind),
            source: Some(source_tag.to_string()),
            resolutions: Vec::new(),
        },
    ));
}

fn resolve_url(base_url: &Url, raw_url: &str) -> Option<Url> {
    let trimmed = raw_url.trim().trim_matches(['"', '\'', '`']);
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with("mailto:")
        || trimmed.starts_with("tel:")
        || trimmed.starts_with("javascript:")
        || trimmed.starts_with("data:")
    {
        return None;
    }

    if trimmed.starts_with("//") {
        return Url::parse(&format!("{}:{trimmed}", base_url.scheme())).ok();
    }

    base_url.join(trimmed).ok()
}

fn classify_candidate(url: &Url, source_tag: &str, label: &str) -> Option<(String, i32)> {
    let raw = url.as_str().to_lowercase();
    let path = url.path().to_lowercase();
    let label = label.to_lowercase();
    let has_download_signal = contains_any(
        &label,
        &[
            "descargar",
            "descarga",
            "download",
            "bajar",
            "servidor",
            "server",
            "opcion",
            "opci",
            "mirror",
        ],
    ) || contains_any(
        &raw,
        &[
            "download",
            "descargar",
            "descarga",
            "server",
            "mirror",
            "/dl/",
            "dl=",
        ],
    );

    if raw.contains(".m3u8") {
        return Some(("HLS playlist".to_string(), 120));
    }
    if raw.contains(".mpd") {
        return Some(("DASH manifest".to_string(), 115));
    }
    if has_extension(
        &path,
        &["mp4", "m4v", "webm", "mkv", "mov", "avi", "flv", "ts"],
    ) {
        return Some(("video".to_string(), 110));
    }
    if has_extension(&path, &["mp3", "m4a", "aac", "ogg", "wav"]) {
        return Some(("audio".to_string(), 90));
    }
    if has_extension(
        &path,
        &[
            "zip", "rar", "7z", "tar", "gz", "pdf", "epub", "doc", "docx", "xls", "xlsx", "apk",
            "exe", "msi", "dmg", "iso",
        ],
    ) {
        return Some(("archivo".to_string(), 88));
    }
    if has_extension(&path, &["srt", "vtt", "ass"]) {
        return Some(("subtitulo".to_string(), 55));
    }
    if has_download_signal {
        let kind = if matches!(source_tag, "button" | "form" | "input") {
            "boton descarga"
        } else {
            "link descarga"
        };
        return Some((kind.to_string(), 84));
    }
    if matches!(source_tag, "video" | "source" | "audio") {
        return Some(("media source".to_string(), 95));
    }
    if matches!(source_tag, "iframe" | "embed") {
        return Some(("reproductor".to_string(), 85));
    }
    if contains_any(
        &raw,
        &["/embed/", "embed-", "player", "stream", "video", "watch"],
    ) {
        return Some(("posible reproductor".to_string(), 70));
    }
    if contains_any(
        &label,
        &[
            "ver online",
            "reproducir",
            "play",
            "trailer",
            "video",
            "stream",
        ],
    ) {
        return Some(("link video".to_string(), 45));
    }

    None
}

fn is_direct_file_url(url: &Url) -> bool {
    let raw = url.as_str().to_lowercase();
    let path = url.path().to_lowercase();

    raw.contains(".m3u8")
        || raw.contains(".mpd")
        || has_extension(
            &path,
            &[
                "mp4", "m4v", "webm", "mkv", "mov", "avi", "flv", "ts", "mp3", "m4a", "aac", "ogg",
                "wav", "srt", "vtt", "ass", "zip", "rar", "7z", "tar", "gz", "pdf", "epub", "doc",
                "docx", "xls", "xlsx", "apk", "exe", "msi", "dmg", "iso",
            ],
        )
}

fn has_extension(path: &str, extensions: &[&str]) -> bool {
    extensions
        .iter()
        .any(|extension| path.ends_with(&format!(".{extension}")))
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn element_label(element: &scraper::ElementRef<'_>, fallback: &str) -> String {
    let text = clean_label(&element.text().collect::<Vec<_>>().join(" "));
    if !text.is_empty() {
        return text;
    }

    for attr in [
        "title",
        "aria-label",
        "alt",
        "value",
        "data-title",
        "data-name",
        "data-label",
    ] {
        if let Some(value) = element.value().attr(attr) {
            let cleaned = clean_label(value);
            if !cleaned.is_empty() {
                return cleaned;
            }
        }
    }

    fallback.to_string()
}

fn clean_label(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compact_url_title(url: &Url) -> String {
    let mut parts = Vec::new();
    if let Some(host) = url.host_str() {
        parts.push(host.to_string());
    }
    if let Some(segment) = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
    {
        parts.push(segment.to_string());
    }
    if parts.is_empty() {
        url.to_string()
    } else {
        parts.join(" / ")
    }
}

fn harvest_urls_from_text(html: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    let patterns = [
        r#"https?:\\?/\\?/[^"'<>\s\\]+"#,
        r#"(?:"|')([^"']+\.(?:m3u8|mpd|mp4|m4v|webm|mkv|mov|avi|flv|ts|mp3|m4a|zip|rar|7z|pdf)(?:\?[^"']*)?)(?:"|')"#,
    ];

    for pattern in patterns {
        let Ok(regex) = Regex::new(pattern) else {
            continue;
        };

        for capture in regex.captures_iter(html) {
            let matched = capture.get(1).or_else(|| capture.get(0));
            let Some(matched) = matched else {
                continue;
            };
            let cleaned = matched
                .as_str()
                .trim_matches(['"', '\''])
                .replace("\\/", "/")
                .replace("&amp;", "&");
            if seen.insert(cleaned.clone()) {
                urls.push(cleaned);
            }
        }
    }

    urls
}

fn harvest_navigation_targets(value: &str) -> Vec<String> {
    let mut urls = harvest_urls_from_text(value);
    let mut seen = urls.iter().cloned().collect::<HashSet<_>>();

    let patterns = [
        r#"(?:location(?:\.href)?|window\.open|open)\s*(?:=|\()\s*["']([^"']+)["']"#,
        r#"(?:url|href|redirect)\s*[:=]\s*["']([^"']+)["']"#,
        r#"["']([^"']*(?:download|descarga|descargar|server|mirror|embed|player|stream|watch)[^"']*)["']"#,
    ];

    for pattern in patterns {
        let Ok(regex) = Regex::new(pattern) else {
            continue;
        };

        for capture in regex.captures_iter(value) {
            let Some(matched) = capture.get(1) else {
                continue;
            };
            let cleaned = matched
                .as_str()
                .trim()
                .replace("\\/", "/")
                .replace("&amp;", "&");
            if seen.insert(cleaned.clone()) {
                urls.push(cleaned);
            }
        }
    }

    urls
}

fn meta_refresh_target(value: &str) -> Option<String> {
    let regex = Regex::new(r#"(?i)\burl\s*=\s*([^;]+)"#).ok()?;
    regex
        .captures(value)
        .and_then(|capture| capture.get(1))
        .map(|matched| {
            matched
                .as_str()
                .trim()
                .trim_matches(['"', '\''])
                .to_string()
        })
        .filter(|target| !target.is_empty())
}

fn value_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|inner| inner.as_str())
        .map(str::trim)
        .filter(|inner| !inner.is_empty())
        .map(ToString::to_string)
}

fn duration_string(value: &Value) -> Option<String> {
    let seconds = value.get("duration")?.as_f64()?;
    let total = seconds.round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;

    if hours > 0 {
        Some(format!("{hours}:{minutes:02}:{seconds:02}"))
    } else {
        Some(format!("{minutes}:{seconds:02}"))
    }
}

fn locate_yt_dlp(app: &AppHandle) -> Result<PathBuf, String> {
    let executable = if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    };

    let mut candidates = Vec::new();

    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        candidates.push(
            PathBuf::from(manifest_dir)
                .join("..")
                .join("tools")
                .join(executable),
        );
    }

    if let Ok(current_dir) = std::env::current_dir() {
        candidates.push(current_dir.join("tools").join(executable));
    }

    if let Ok(resource_dir) = app.path().resource_dir() {
        candidates.push(resource_dir.join("tools").join(executable));
        candidates.push(resource_dir.join(executable));
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            candidates.push(parent.join("tools").join(executable));
            candidates.push(parent.join(executable));
        }
    }

    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    locate_path_command("yt-dlp").ok_or_else(|| {
        "No encontré yt-dlp. Dejalo en tools\\yt-dlp.exe o instalalo en el PATH.".to_string()
    })
}

fn append_runtime_args(args: &mut Vec<String>) {
    if let Some(node) = locate_node() {
        args.push("--js-runtimes".to_string());
        args.push(format!("node:{}", node.to_string_lossy()));
    }
}

fn format_selector(max_height: Option<u32>) -> String {
    let Some(height) = max_height.filter(|height| *height > 0) else {
        return "bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/bv*+ba/b".to_string();
    };
    let height = height.min(16_384);

    format!(
        "bv*[height<={height}][ext=mp4]+ba[ext=m4a]/b[height<={height}][ext=mp4]/bv*[height<={height}]+ba/b[height<={height}]"
    )
}

fn append_ffmpeg_args(args: &mut Vec<String>) {
    if let Some(ffmpeg) = locate_ffmpeg() {
        let location = ffmpeg.parent().unwrap_or(ffmpeg.as_path());
        args.push("--ffmpeg-location".to_string());
        args.push(location.to_string_lossy().to_string());
    }
}

fn append_browser_args(args: &mut Vec<String>, browser: Option<&str>) -> Result<(), String> {
    let Some(browser) = browser.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };

    if browser.eq_ignore_ascii_case("none") {
        return Ok(());
    }

    let normalized = browser.to_ascii_lowercase();
    if !matches!(normalized.as_str(), "brave" | "chrome" | "edge" | "firefox") {
        return Err("Navegador de cookies no compatible.".to_string());
    }

    args.push("--cookies-from-browser".to_string());
    args.push(normalized);
    Ok(())
}

fn locate_node() -> Option<PathBuf> {
    locate_path_command("node").or_else(|| {
        let program_files = std::env::var_os("ProgramFiles")?;
        let candidate = PathBuf::from(program_files).join("nodejs").join("node.exe");
        candidate.exists().then_some(candidate)
    })
}

fn locate_ffmpeg() -> Option<PathBuf> {
    locate_path_command("ffmpeg").or_else(|| locate_winget_binary("Gyan.FFmpeg", "ffmpeg.exe"))
}

fn locate_winget_binary(package_prefix: &str, executable: &str) -> Option<PathBuf> {
    let packages = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("Microsoft")
        .join("WinGet")
        .join("Packages");

    for package in fs::read_dir(packages).ok()?.flatten() {
        if !package
            .file_name()
            .to_string_lossy()
            .starts_with(package_prefix)
        {
            continue;
        }

        for distribution in fs::read_dir(package.path()).ok()?.flatten() {
            let candidate = distribution.path().join("bin").join(executable);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    None
}

fn locate_edge() -> Option<PathBuf> {
    ["ProgramFiles(x86)", "ProgramFiles"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .map(|root| {
            root.join("Microsoft")
                .join("Edge")
                .join("Application")
                .join("msedge.exe")
        })
        .find(|candidate| candidate.exists())
}

fn locate_path_command(name: &str) -> Option<PathBuf> {
    which::which(name).ok()
}

fn default_output_dir() -> Option<String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;

    Some(
        PathBuf::from(home)
            .join("Downloads")
            .join("Descargador A1")
            .to_string_lossy()
            .to_string(),
    )
}

fn normalize_output_dir(output_dir: Option<String>) -> Result<Option<PathBuf>, String> {
    let Some(raw) = output_dir else {
        return Ok(default_output_dir().map(PathBuf::from));
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(default_output_dir().map(PathBuf::from));
    }

    Ok(Some(PathBuf::from(trimmed)))
}

fn clean_process_error(stderr: &[u8], fallback: &str) -> String {
    let error = String::from_utf8_lossy(stderr);
    let cleaned = error.trim();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.to_string()
    }
}

fn main() {
    tauri::Builder::default()
        .manage(DownloadState::default())
        .invoke_handler(tauri::generate_handler![
            check_tools,
            probe_url,
            scan_page,
            start_download,
            cancel_download
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_arguments_are_allowlisted() {
        let mut args = Vec::new();
        append_browser_args(&mut args, Some("edge")).unwrap();
        assert_eq!(args, ["--cookies-from-browser", "edge"]);
        assert!(append_browser_args(&mut args, Some("unknown")).is_err());
    }

    #[test]
    fn final_file_line_completes_the_job() {
        let update =
            parse_download_line("job-1", "[download] Final file: C:\\Downloads\\video.mp4")
                .unwrap();

        assert_eq!(update.status, "completed");
        assert_eq!(update.progress, Some(100.0));
        assert_eq!(update.file.as_deref(), Some("C:\\Downloads\\video.mp4"));
    }

    #[test]
    fn scanner_finds_media_urls_inside_scripts() {
        let base = Url::parse("https://example.com/watch").unwrap();
        let html = r#"<script>const source = "https://cdn.example.com/video.mp4";</script>"#;
        let entries = extract_page_candidates(&base, html);

        assert!(entries
            .iter()
            .any(|entry| entry.url == "https://cdn.example.com/video.mp4"));
    }

    #[test]
    fn resolutions_are_unique_and_sorted_highest_first() {
        let value = serde_json::json!({
            "formats": [
                { "height": 720, "vcodec": "avc1" },
                { "height": 1080, "vcodec": "av01" },
                { "height": 720, "vcodec": "vp9" },
                { "vcodec": "none" }
            ]
        });

        assert_eq!(extract_resolutions(&value), [1080, 720]);
        assert!(format_selector(Some(720)).contains("height<=720"));
    }
}

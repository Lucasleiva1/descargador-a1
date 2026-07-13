use regex::Regex;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    io::Write,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, Notify, Semaphore},
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
    updates: Arc<Mutex<HashMap<String, DownloadUpdate>>>,
}

#[derive(Default)]
struct SearchState {
    cancelled: AtomicBool,
    process_id: Mutex<Option<u32>>,
    notify: Notify,
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
    choice_group: Option<String>,
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

#[derive(Debug, Serialize)]
struct DownloadSnapshot {
    active: bool,
    updates: Vec<DownloadUpdate>,
}

#[derive(Debug, Serialize)]
struct YoutubeSessionState {
    active: bool,
    cookie_count: usize,
    message: String,
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
async fn open_youtube_login(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("youtube-login") {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        return Ok(());
    }

    let profile_dir = youtube_session_profile_dir(&app)?;
    fs::create_dir_all(&profile_dir)
        .map_err(|error| format!("No pude preparar la sesion de YouTube: {error}"))?;
    let login_url = Url::parse(
        "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fwww.youtube.com%2F",
    )
    .map_err(|error| error.to_string())?;

    WebviewWindowBuilder::new(&app, "youtube-login", WebviewUrl::External(login_url))
        .title("Sesion de YouTube - Descargador A1")
        .inner_size(1100.0, 760.0)
        .min_inner_size(760.0, 560.0)
        .data_directory(profile_dir)
        .build()
        .map_err(|error| format!("No pude abrir el inicio de sesion: {error}"))?;

    Ok(())
}

#[tauri::command]
async fn save_youtube_session(app: AppHandle) -> Result<YoutubeSessionState, String> {
    let window = app
        .get_webview_window("youtube-login")
        .ok_or_else(|| "Abri primero la ventana de inicio de sesion.".to_string())?;

    let cookies = tokio::task::spawn_blocking(move || window.cookies())
        .await
        .map_err(|error| format!("No pude leer la sesion: {error}"))?
        .map_err(|error| format!("No pude leer la sesion: {error}"))?;

    let relevant = cookies
        .into_iter()
        .filter(|cookie| {
            cookie.domain().is_some_and(|domain| {
                domain.ends_with("youtube.com") || domain.ends_with("google.com")
            })
        })
        .collect::<Vec<_>>();

    if !relevant.iter().any(|cookie| {
        matches!(
            cookie.name(),
            "SID" | "SAPISID" | "__Secure-1PSID" | "__Secure-3PSID"
        )
    }) {
        return Err("Todavia no detecte una cuenta de YouTube iniciada.".to_string());
    }

    let cookie_file = youtube_session_cookie_file(&app)?;
    if let Some(parent) = cookie_file.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("No pude guardar la sesion: {error}"))?;
    }

    let mut contents = String::from("# Netscape HTTP Cookie File\n");
    for cookie in &relevant {
        let Some(domain) = cookie.domain() else {
            continue;
        };
        let include_subdomains = if domain.starts_with('.') {
            "TRUE"
        } else {
            "FALSE"
        };
        let path = cookie.path().unwrap_or("/");
        let secure = if cookie.secure().unwrap_or(false) {
            "TRUE"
        } else {
            "FALSE"
        };
        let expires = cookie
            .expires_datetime()
            .map(|date| date.unix_timestamp().max(0))
            .unwrap_or(0);
        let stored_domain = if cookie.http_only().unwrap_or(false) {
            format!("#HttpOnly_{domain}")
        } else {
            domain.to_string()
        };
        let value = cookie.value().replace(['\t', '\r', '\n'], "");
        contents.push_str(&format!(
            "{stored_domain}\t{include_subdomains}\t{path}\t{secure}\t{expires}\t{}\t{value}\n",
            cookie.name()
        ));
    }

    fs::write(&cookie_file, contents)
        .map_err(|error| format!("No pude guardar la sesion: {error}"))?;

    Ok(YoutubeSessionState {
        active: true,
        cookie_count: relevant.len(),
        message: "Sesion de YouTube lista.".to_string(),
    })
}

#[tauri::command]
fn get_youtube_session(app: AppHandle) -> YoutubeSessionState {
    let active = youtube_session_cookie_file(&app).is_ok_and(|path| path.is_file());
    YoutubeSessionState {
        active,
        cookie_count: 0,
        message: if active {
            "Sesion de YouTube lista.".to_string()
        } else {
            "Sin sesion de YouTube.".to_string()
        },
    }
}

#[tauri::command]
async fn cancel_search(search: State<'_, SearchState>) -> Result<bool, String> {
    search.cancelled.store(true, Ordering::SeqCst);
    search.notify.notify_waiters();
    let process_id = search.process_id.lock().await.take();

    if let Some(process_id) = process_id {
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &process_id.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        return Ok(true);
    }

    Ok(false)
}

#[tauri::command]
async fn probe_url(
    app: AppHandle,
    search: State<'_, SearchState>,
    url: String,
    browser: Option<String>,
) -> Result<ProbeResult, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("Pegá una URL válida.".to_string());
    }

    let yt_dlp = locate_yt_dlp(&app)?;
    search.cancelled.store(false, Ordering::SeqCst);
    if search.process_id.lock().await.is_some() {
        return Err("Ya hay una busqueda activa.".to_string());
    }

    let mut args = vec![
        "--dump-single-json".to_string(),
        "--no-color".to_string(),
        "--ignore-config".to_string(),
        "--socket-timeout".to_string(),
        "30".to_string(),
        "--retries".to_string(),
        "3".to_string(),
    ];
    if is_single_video_url(&url) {
        args.push("--no-playlist".to_string());
    }
    append_runtime_args(&mut args);
    append_managed_session_args(&app, &mut args, browser.as_deref())?;
    append_browser_args(&mut args, browser.as_deref())?;
    args.push(url.clone());

    let mut command = Command::new(&yt_dlp);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|error| format!("No pude ejecutar yt-dlp: {error}"))?;
    let process_id = child.id();
    *search.process_id.lock().await = process_id;
    let output = child.wait_with_output().await;
    *search.process_id.lock().await = None;
    let output = output.map_err(|error| format!("No pude esperar a yt-dlp: {error}"))?;

    if search.cancelled.load(Ordering::SeqCst) {
        return Err("Busqueda detenida.".to_string());
    }

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
async fn scan_page(
    search: State<'_, SearchState>,
    url: String,
    referer: Option<String>,
) -> Result<ProbeResult, String> {
    let parsed_url = Url::parse(url.trim())
        .map_err(|error| format!("No pude interpretar esa URL para escanear la pagina: {error}"))?;

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(35))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(SCANNER_USER_AGENT)
        .build()
        .map_err(|error| format!("No pude preparar el escaner: {error}"))?;

    let (final_url, html) = cancellable_fetch_html(
        &client,
        &parsed_url,
        referer
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        &search,
    )
    .await?;

    let title = extract_page_title(&html);
    let mut entries = extract_page_candidates(&final_url, &html);
    let mut seen = entries
        .iter()
        .map(|entry| entry.url.clone())
        .collect::<HashSet<_>>();

    match discover_dynamic_player_candidates(&client, &final_url, &html, &search).await {
        Ok(dynamic_entries) => {
            for entry in dynamic_entries {
                if seen.insert(entry.url.clone()) {
                    entries.push(entry);
                }
            }
        }
        Err(error) if search.cancelled.load(Ordering::SeqCst) => return Err(error),
        Err(_) => {}
    }

    if let Some(rendered_html) = render_page(&final_url, &search).await {
        for entry in extract_page_candidates(&final_url, &rendered_html) {
            if seen.insert(entry.url.clone()) {
                entries.push(entry);
            }
        }
    }

    if search.cancelled.load(Ordering::SeqCst) {
        return Err("Busqueda detenida.".to_string());
    }

    let follow_targets = entries
        .iter()
        .filter_map(|entry| follow_target(entry, &final_url))
        .take(MAX_FOLLOW_LINKS)
        .collect::<Vec<_>>();

    for target in follow_targets {
        tokio::time::sleep(Duration::from_millis(FOLLOW_DELAY_MS)).await;

        let Ok((detail_url, detail_html)) =
            cancellable_fetch_html(&client, &target, Some(final_url.as_str()), &search).await
        else {
            if search.cancelled.load(Ordering::SeqCst) {
                return Err("Busqueda detenida.".to_string());
            }
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

async fn discover_dynamic_player_candidates(
    client: &reqwest::Client,
    page_url: &Url,
    html: &str,
    search: &SearchState,
) -> Result<Vec<ProbeEntry>, String> {
    let fast_api_regex =
        Regex::new(r#"fastApi\s*:\s*['\"]([^'\"]+)['\"]"#).map_err(|error| error.to_string())?;
    let Some(capture) = fast_api_regex.captures(html) else {
        return Ok(Vec::new());
    };
    let Some(base_match) = capture.get(1) else {
        return Ok(Vec::new());
    };
    let base = Url::parse(&format!("{}/", base_match.as_str().trim_end_matches('/')))
        .map_err(|error| error.to_string())?;
    if !matches!(base.scheme(), "http" | "https") || base.host_str() != page_url.host_str() {
        return Ok(Vec::new());
    }

    let Some(slug) = page_url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
    else {
        return Ok(Vec::new());
    };

    let mut post_id = None;
    for post_type in ["movies", "tvshows", "animes"] {
        let mut endpoint = base
            .join(&format!("single/{post_type}"))
            .map_err(|error| error.to_string())?;
        endpoint
            .query_pairs_mut()
            .append_pair("slug", slug)
            .append_pair("postType", post_type);

        let Ok(response) = cancellable_fetch_json(client, &endpoint, page_url, search).await else {
            if search.cancelled.load(Ordering::SeqCst) {
                return Err("Busqueda detenida.".to_string());
            }
            continue;
        };
        let data = response.get("data").unwrap_or(&response);
        post_id = value_identifier(data.get("_id"));
        if post_id.is_some() {
            break;
        }
    }

    let Some(post_id) = post_id else {
        return Ok(Vec::new());
    };
    let mut player_endpoint = base.join("player").map_err(|error| error.to_string())?;
    player_endpoint
        .query_pairs_mut()
        .append_pair("postId", &post_id)
        .append_pair("demo", "0");
    let response = cancellable_fetch_json(client, &player_endpoint, page_url, search).await?;
    let data = response.get("data").unwrap_or(&response);

    Ok(player_entries(data, Some(page_url.as_str())))
}

fn player_entries(data: &Value, choice_group: Option<&str>) -> Vec<ProbeEntry> {
    let mut entries = Vec::new();
    let has_downloads = data
        .get("downloads")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|url| url.starts_with("http://") || url.starts_with("https://"))
            })
        });
    let fields = if has_downloads {
        [("downloads", "Fuente de descarga")].as_slice()
    } else {
        [("embeds", "Reproductor")].as_slice()
    };

    for (field, kind) in fields {
        let Some(items) = data.get(field).and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let raw_url = item
                .get("url")
                .and_then(Value::as_str)
                .or_else(|| item.as_str());
            let Some(raw_url) = raw_url else {
                continue;
            };
            let Ok(url) = Url::parse(raw_url) else {
                continue;
            };
            if !matches!(url.scheme(), "http" | "https") {
                continue;
            }
            let host = url
                .host_str()
                .unwrap_or("Fuente")
                .trim_start_matches("www.");
            let quality = item
                .get("quality")
                .and_then(Value::as_str)
                .map(clean_label)
                .filter(|value| !value.is_empty());
            let language = item
                .get("lang")
                .and_then(Value::as_str)
                .map(clean_label)
                .filter(|value| !value.is_empty());
            let label = [
                Some(host.to_string()),
                source_format_label(&url),
                quality,
                language,
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" - ");
            let url = url.to_string();
            entries.push(ProbeEntry {
                id: None,
                title: Some(label),
                url: url.clone(),
                webpage_url: Some(url),
                duration: None,
                kind: Some((*kind).to_string()),
                source: Some(
                    if *field == "downloads" {
                        "dynamic-download"
                    } else {
                        "dynamic-player"
                    }
                    .to_string(),
                ),
                choice_group: choice_group.map(str::to_string),
                resolutions: Vec::new(),
            });
        }
    }

    entries.sort_by_key(|entry| source_preference(&entry.url));

    entries
}

fn source_preference(raw_url: &str) -> u8 {
    let host = Url::parse(raw_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_lowercase))
        .unwrap_or_default();
    if host.contains("mediafire.com") {
        0
    } else if host.contains("megaup.net") {
        1
    } else if host.contains("mega.nz") {
        2
    } else if host.contains("1fichier.com") {
        3
    } else {
        10
    }
}

fn source_format_label(url: &Url) -> Option<String> {
    let path = url.path().to_ascii_lowercase();
    if path.contains(".rar/") || path.ends_with(".rar") {
        Some("RAR - requiere extraer".to_string())
    } else if path.ends_with(".mkv") {
        Some("Video MKV".to_string())
    } else if path.ends_with(".mp4") {
        Some("Video MP4".to_string())
    } else {
        None
    }
}

fn value_identifier(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

async fn cancellable_fetch_json(
    client: &reqwest::Client,
    url: &Url,
    referer: &Url,
    search: &SearchState,
) -> Result<Value, String> {
    if search.cancelled.load(Ordering::SeqCst) {
        return Err("Busqueda detenida.".to_string());
    }

    let request = async {
        let response = client
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::REFERER, referer.as_str())
            .send()
            .await
            .map_err(|error| format!("No pude consultar el reproductor: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "El reproductor respondio con estado HTTP {}.",
                response.status()
            ));
        }
        let raw = response
            .text()
            .await
            .map_err(|error| format!("No pude leer la respuesta del reproductor: {error}"))?;
        serde_json::from_str::<Value>(&raw)
            .map_err(|error| format!("El reproductor devolvio JSON invalido: {error}"))
    };

    tokio::select! {
        result = request => result,
        _ = search.notify.notified() => Err("Busqueda detenida.".to_string()),
    }
}

async fn render_page(url: &Url, search: &SearchState) -> Option<String> {
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

    let child = command.spawn().ok()?;
    *search.process_id.lock().await = child.id();
    let output = tokio::time::timeout(Duration::from_secs(35), child.wait_with_output()).await;
    *search.process_id.lock().await = None;
    let _ = fs::remove_dir_all(profile);
    if search.cancelled.load(Ordering::SeqCst) {
        return None;
    }
    let output = output.ok()?.ok()?;

    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
        .filter(|html| !html.trim().is_empty())
}

async fn cancellable_fetch_html(
    client: &reqwest::Client,
    url: &Url,
    referer: Option<&str>,
    search: &SearchState,
) -> Result<(Url, String), String> {
    if search.cancelled.load(Ordering::SeqCst) {
        return Err("Busqueda detenida.".to_string());
    }

    tokio::select! {
        result = fetch_html(client, url, referer) => result,
        _ = search.notify.notified() => Err("Busqueda detenida.".to_string()),
    }
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
    concurrency: Option<usize>,
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
    let updates = state.updates.clone();
    let concurrency = normalize_concurrency(concurrency);
    {
        let mut controls = job_controls.lock().await;
        let mut current_updates = updates.lock().await;
        for job in &jobs {
            controls.insert(job.id.clone(), Arc::new(JobCancellation::default()));
            current_updates.insert(
                job.id.clone(),
                DownloadUpdate {
                    id: job.id.clone(),
                    status: "extracting".to_string(),
                    progress: Some(0.0),
                    speed: None,
                    eta: None,
                    file: None,
                    message: Some("Preparando descarga...".to_string()),
                },
            );
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

        let semaphore = Arc::new(Semaphore::new(concurrency));
        let mut handles = Vec::with_capacity(jobs.len());

        for job in jobs {
            let Ok(permit) = semaphore.clone().acquire_owned().await else {
                break;
            };
            let task_app = app.clone();
            let task_yt_dlp = yt_dlp.clone();
            let task_output_dir = output_dir.clone();
            let task_referer = referer.clone();
            let task_browser = browser.clone();
            let task_controls = job_controls.clone();
            let task_updates = updates.clone();

            handles.push(tauri::async_runtime::spawn(async move {
                let _permit = permit;
                let cancellation = {
                    let controls = task_controls.lock().await;
                    controls.get(&job.id).cloned()
                };

                if let Some(cancellation) = cancellation {
                    if cancellation.is_cancelled() {
                        emit_cancelled(&task_app, &task_updates, &job.id).await;
                    } else {
                        run_download_job(
                            &task_app,
                            &task_yt_dlp,
                            &job,
                            task_output_dir.as_ref(),
                            task_referer.as_deref(),
                            task_browser.as_deref(),
                            cancellation,
                            task_updates.clone(),
                        )
                        .await;
                    }
                }

                task_controls.lock().await.remove(&job.id);
            }));
        }

        for handle in handles {
            let _ = handle.await;
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
async fn get_download_snapshot(
    state: State<'_, DownloadState>,
) -> Result<DownloadSnapshot, String> {
    Ok(DownloadSnapshot {
        active: state.active.load(Ordering::SeqCst),
        updates: state.updates.lock().await.values().cloned().collect(),
    })
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

#[allow(clippy::too_many_arguments)]
async fn run_download_job(
    app: &AppHandle,
    yt_dlp: &PathBuf,
    job: &DownloadJob,
    output_dir: Option<&PathBuf>,
    referer: Option<&str>,
    browser: Option<&str>,
    cancellation: Arc<JobCancellation>,
    updates: Arc<Mutex<HashMap<String, DownloadUpdate>>>,
) {
    publish_update(
        app,
        &updates,
        DownloadUpdate {
            id: job.id.clone(),
            status: "extracting".to_string(),
            progress: Some(0.0),
            speed: None,
            eta: None,
            file: None,
            message: job.title.clone().or_else(|| Some(job.url.clone())),
        },
    )
    .await;

    if is_mediafire_page(&job.url) {
        run_mediafire_download_job(app, job, output_dir, cancellation, updates).await;
        return;
    }

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
    if let Err(error) = append_managed_session_args(app, &mut args, browser) {
        publish_update(
            app,
            &updates,
            DownloadUpdate {
                id: job.id.clone(),
                status: "failed".to_string(),
                progress: None,
                speed: None,
                eta: None,
                file: None,
                message: Some(error),
            },
        )
        .await;
        return;
    }
    if let Err(error) = append_browser_args(&mut args, browser) {
        publish_update(
            app,
            &updates,
            DownloadUpdate {
                id: job.id.clone(),
                status: "failed".to_string(),
                progress: None,
                speed: None,
                eta: None,
                file: None,
                message: Some(error),
            },
        )
        .await;
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
            publish_update(
                app,
                &updates,
                DownloadUpdate {
                    id: job.id.clone(),
                    status: "failed".to_string(),
                    progress: None,
                    speed: None,
                    eta: None,
                    file: None,
                    message: Some(format!("No pude iniciar yt-dlp: {error}")),
                },
            )
            .await;
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_app = app.clone();
    let stderr_app = app.clone();
    let stdout_job = job.id.clone();
    let stderr_job = job.id.clone();
    let stdout_updates = updates.clone();
    let stderr_updates = updates.clone();

    let stdout_task = stdout.map(|stream| {
        tokio::spawn(async move {
            read_process_stream(stdout_app, stdout_job, stream, stdout_updates).await;
        })
    });

    let stderr_task = stderr.map(|stream| {
        tokio::spawn(async move {
            read_process_stream(stderr_app, stderr_job, stream, stderr_updates).await
        })
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
        None => emit_cancelled(app, &updates, &job.id).await,
        Some(Ok(status)) if status.success() => {
            let final_file = updates
                .lock()
                .await
                .get(&job.id)
                .and_then(|update| update.file.clone());
            let empty_file = final_file
                .as_ref()
                .and_then(|path| fs::metadata(path).ok())
                .is_some_and(|metadata| metadata.len() == 0);

            publish_update(
                app,
                &updates,
                DownloadUpdate {
                    id: job.id.clone(),
                    status: if empty_file { "failed" } else { "completed" }.to_string(),
                    progress: if empty_file { None } else { Some(100.0) },
                    speed: None,
                    eta: None,
                    file: final_file,
                    message: Some(if empty_file {
                        "El archivo resultante esta vacio; volve a iniciar la descarga.".to_string()
                    } else {
                        "Descarga completa.".to_string()
                    }),
                },
            )
            .await;
        }
        Some(Ok(status)) => {
            publish_update(
                app,
                &updates,
                DownloadUpdate {
                    id: job.id.clone(),
                    status: "failed".to_string(),
                    progress: None,
                    speed: None,
                    eta: None,
                    file: None,
                    message: Some(
                        stderr_message
                            .unwrap_or_else(|| format!("yt-dlp terminó con código {status}")),
                    ),
                },
            )
            .await;
        }
        Some(Err(error)) => {
            publish_update(
                app,
                &updates,
                DownloadUpdate {
                    id: job.id.clone(),
                    status: "failed".to_string(),
                    progress: None,
                    speed: None,
                    eta: None,
                    file: None,
                    message: Some(format!("Error esperando a yt-dlp: {error}")),
                },
            )
            .await;
        }
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

async fn run_mediafire_download_job(
    app: &AppHandle,
    job: &DownloadJob,
    output_dir: Option<&PathBuf>,
    cancellation: Arc<JobCancellation>,
    updates: Arc<Mutex<HashMap<String, DownloadUpdate>>>,
) {
    let result =
        download_mediafire_file(app, job, output_dir, cancellation.clone(), updates.clone()).await;

    match result {
        Ok(file) => {
            publish_update(
                app,
                &updates,
                DownloadUpdate {
                    id: job.id.clone(),
                    status: "completed".to_string(),
                    progress: Some(100.0),
                    speed: None,
                    eta: Some("0s".to_string()),
                    file: Some(file.clone()),
                    message: Some(format!("Archivo guardado: {file}")),
                },
            )
            .await;
        }
        Err(_error) if cancellation.is_cancelled() => {
            emit_cancelled(app, &updates, &job.id).await;
        }
        Err(error) => {
            publish_update(
                app,
                &updates,
                DownloadUpdate {
                    id: job.id.clone(),
                    status: "failed".to_string(),
                    progress: None,
                    speed: None,
                    eta: None,
                    file: None,
                    message: Some(error),
                },
            )
            .await;
        }
    }
}

async fn download_mediafire_file(
    app: &AppHandle,
    job: &DownloadJob,
    output_dir: Option<&PathBuf>,
    cancellation: Arc<JobCancellation>,
    updates: Arc<Mutex<HashMap<String, DownloadUpdate>>>,
) -> Result<String, String> {
    let page_url =
        Url::parse(&job.url).map_err(|error| format!("URL de MediaFire invalida: {error}"))?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(SCANNER_USER_AGENT)
        .build()
        .map_err(|error| format!("No pude preparar la descarga directa: {error}"))?;
    let page_response = client
        .get(page_url.clone())
        .send()
        .await
        .map_err(|error| format!("No pude abrir MediaFire: {error}"))?;
    if !page_response.status().is_success() {
        return Err(format!(
            "MediaFire respondio con estado HTTP {}.",
            page_response.status()
        ));
    }
    let html = page_response
        .text()
        .await
        .map_err(|error| format!("No pude leer la pagina de MediaFire: {error}"))?;
    let direct_url = mediafire_download_url(&page_url, &html)
        .ok_or_else(|| "MediaFire no expuso el boton del archivo.".to_string())?;

    let mut response = client
        .get(direct_url.clone())
        .header(reqwest::header::REFERER, page_url.as_str())
        .send()
        .await
        .map_err(|error| format!("No pude iniciar la transferencia de MediaFire: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "El archivo de MediaFire respondio con estado HTTP {}.",
            response.status()
        ));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.contains("text/html") {
        return Err("MediaFire devolvio otra pagina en lugar del archivo.".to_string());
    }

    let total = response.content_length().unwrap_or(0);
    let file_name = response_filename(&response, &direct_url);
    let directory = output_dir
        .cloned()
        .or_else(|| default_output_dir().map(PathBuf::from))
        .ok_or_else(|| "No pude determinar la carpeta de salida.".to_string())?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("No pude crear la carpeta de salida: {error}"))?;
    let final_path = available_output_path(directory.join(file_name));
    let part_path = PathBuf::from(format!("{}.part", final_path.to_string_lossy()));
    let mut file = fs::File::create(&part_path)
        .map_err(|error| format!("No pude crear el archivo temporal: {error}"))?;
    let started = Instant::now();
    let mut last_update = Instant::now();
    let mut downloaded = 0_u64;

    let transfer_result: Result<(), String> = async {
        loop {
            if cancellation.is_cancelled() {
                return Err("Descarga detenida.".to_string());
            }
            let chunk = tokio::select! {
                _ = cancellation.notify.notified() => Err("Descarga detenida.".to_string()),
                result = response.chunk() => result
                    .map_err(|error| format!("Se interrumpio la transferencia: {error}")),
            }?;
            let Some(chunk) = chunk else {
                break;
            };
            file.write_all(&chunk)
                .map_err(|error| format!("No pude escribir el archivo: {error}"))?;
            downloaded += chunk.len() as u64;

            if last_update.elapsed() >= Duration::from_millis(350) {
                let elapsed = started.elapsed().as_secs_f64().max(0.1);
                let bytes_per_second = downloaded as f64 / elapsed;
                let progress = if total > 0 {
                    downloaded as f64 * 100.0 / total as f64
                } else {
                    0.0
                };
                let eta = if total > downloaded && bytes_per_second > 0.0 {
                    Some(format_duration(
                        ((total - downloaded) as f64 / bytes_per_second) as u64,
                    ))
                } else {
                    None
                };
                publish_update(
                    app,
                    &updates,
                    DownloadUpdate {
                        id: job.id.clone(),
                        status: "downloading".to_string(),
                        progress: Some(progress.min(99.9)),
                        speed: Some(format!("{}/s", format_bytes(bytes_per_second as u64))),
                        eta,
                        file: None,
                        message: Some(format!(
                            "MediaFire: {}",
                            final_path.file_name().unwrap_or_default().to_string_lossy()
                        )),
                    },
                )
                .await;
                last_update = Instant::now();
            }
        }
        Ok(())
    }
    .await;

    if let Err(error) = transfer_result {
        drop(file);
        let _ = fs::remove_file(&part_path);
        return Err(error);
    }
    if let Err(error) = file.flush() {
        drop(file);
        let _ = fs::remove_file(&part_path);
        return Err(format!("No pude terminar de escribir el archivo: {error}"));
    }
    drop(file);
    fs::rename(&part_path, &final_path)
        .map_err(|error| format!("No pude completar el archivo descargado: {error}"))?;
    Ok(final_path.to_string_lossy().to_string())
}

fn is_mediafire_page(raw_url: &str) -> bool {
    Url::parse(raw_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "mediafire.com" || host.ends_with(".mediafire.com"))
}

fn mediafire_download_url(page_url: &Url, html: &str) -> Option<Url> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a#downloadButton[href]").ok()?;
    let href = document.select(&selector).next()?.value().attr("href")?;
    page_url.join(href).ok()
}

fn response_filename(response: &reqwest::Response, url: &Url) -> String {
    let disposition = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let from_header = disposition.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        key.eq_ignore_ascii_case("filename")
            .then(|| value.trim_matches(['"', '\'']).to_string())
    });
    let candidate = from_header
        .or_else(|| url.path_segments()?.next_back().map(str::to_string))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "descarga.bin".to_string());
    sanitize_filename(&candidate)
}

fn sanitize_filename(value: &str) -> String {
    let cleaned = value
        .chars()
        .map(|character| {
            if character.is_control() || "<>:\"/\\|?*".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_string();
    if cleaned.is_empty() {
        "descarga.bin".to_string()
    } else {
        cleaned
    }
}

fn available_output_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let parent = path.parent().map(PathBuf::from).unwrap_or_default();
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("descarga");
    let extension = path.extension().and_then(|value| value.to_str());
    for index in 2..10_000 {
        let name = match extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{stem} ({index})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

async fn emit_cancelled(
    app: &AppHandle,
    updates: &Arc<Mutex<HashMap<String, DownloadUpdate>>>,
    id: &str,
) {
    publish_update(
        app,
        updates,
        DownloadUpdate {
            id: id.to_string(),
            status: "cancelled".to_string(),
            progress: None,
            speed: None,
            eta: None,
            file: None,
            message: Some("Descarga detenida.".to_string()),
        },
    )
    .await;
}

async fn read_process_stream<R>(
    app: AppHandle,
    job_id: String,
    stream: R,
    updates: Arc<Mutex<HashMap<String, DownloadUpdate>>>,
) -> Option<String>
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
            publish_update(&app, &updates, update).await;
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

async fn publish_update(
    app: &AppHandle,
    updates: &Arc<Mutex<HashMap<String, DownloadUpdate>>>,
    update: DownloadUpdate,
) {
    updates
        .lock()
        .await
        .insert(update.id.clone(), update.clone());
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
            choice_group: None,
            resolutions: extract_resolutions(&value),
        });
    }

    ProbeResult {
        source_url: source_url.to_string(),
        title,
        entries,
    }
}

fn is_single_video_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();

    if host == "youtu.be" || host.ends_with(".youtu.be") {
        return !url.path().trim_matches('/').is_empty();
    }

    (host == "youtube.com" || host.ends_with(".youtube.com"))
        && url.path().eq_ignore_ascii_case("/watch")
        && url
            .query_pairs()
            .any(|(key, value)| key == "v" && !value.trim().is_empty())
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
        choice_group: None,
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
    if entry.source.as_deref() == Some("dynamic-download") {
        return None;
    }
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
            choice_group: None,
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
    args.push("--extractor-args".to_string());
    args.push("youtube:player_client=web_safari,android_vr".to_string());
}

fn youtube_session_profile_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_local_data_dir()
        .map(|path| path.join("youtube-session"))
        .map_err(|error| format!("No pude ubicar los datos de la aplicacion: {error}"))
}

fn youtube_session_cookie_file(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_local_data_dir()
        .map(|path| path.join("youtube-cookies.txt"))
        .map_err(|error| format!("No pude ubicar los datos de la aplicacion: {error}"))
}

fn append_managed_session_args(
    app: &AppHandle,
    args: &mut Vec<String>,
    browser: Option<&str>,
) -> Result<(), String> {
    if !browser.is_some_and(|value| matches!(value.trim(), "app" | "auto")) {
        return Ok(());
    }

    let cookie_file = youtube_session_cookie_file(app)?;
    if cookie_file.is_file() {
        args.push("--cookies".to_string());
        args.push(cookie_file.to_string_lossy().to_string());
    }
    Ok(())
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

fn normalize_concurrency(value: Option<usize>) -> usize {
    value.unwrap_or(1).clamp(1, 10)
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

    if matches!(
        browser.to_ascii_lowercase().as_str(),
        "none" | "app" | "auto"
    ) {
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
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(DownloadState::default())
        .manage(SearchState::default())
        .invoke_handler(tauri::generate_handler![
            check_tools,
            open_youtube_login,
            save_youtube_session,
            get_youtube_session,
            cancel_search,
            probe_url,
            scan_page,
            start_download,
            cancel_download,
            get_download_snapshot
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
        append_browser_args(&mut args, Some("app")).unwrap();
        assert!(append_browser_args(&mut args, Some("unknown")).is_err());
    }

    #[test]
    fn youtube_clients_avoid_the_drm_only_default() {
        let mut args = Vec::new();
        append_runtime_args(&mut args);

        assert!(args
            .iter()
            .any(|value| value == "youtube:player_client=web_safari,android_vr"));
    }

    #[test]
    fn youtube_watch_radio_is_treated_as_one_video() {
        assert!(is_single_video_url(
            "https://www.youtube.com/watch?v=YDL8HbY9ENU&list=RDYDL8HbY9ENU&start_radio=1&t=71s"
        ));
        assert!(!is_single_video_url(
            "https://www.youtube.com/playlist?list=PL123"
        ));
    }

    #[test]
    fn dynamic_player_json_prefers_download_sources_without_duplicates() {
        let value = serde_json::json!({
            "embeds": [{ "lang": "Latino", "url": "https://video.example/embed/123" }],
            "downloads": [
                { "label": "MP4", "url": "https://cdn.example/movie.mp4" },
                { "label": "Torrent", "url": "magnet:?xt=urn:test" }
            ]
        });
        let entries = player_entries(&value, Some("https://example.com/movie"));

        assert_eq!(entries.len(), 1);
        assert!(entries
            .iter()
            .all(|entry| entry.kind.as_deref() == Some("Fuente de descarga")));
        assert!(entries.iter().any(|entry| entry.url.ends_with("movie.mp4")));
        assert!(entries
            .iter()
            .all(|entry| entry.choice_group.as_deref() == Some("https://example.com/movie")));
    }

    #[test]
    fn mediafire_button_resolves_to_the_real_file() {
        let page = Url::parse("https://www.mediafire.com/file/example/movie.rar/file").unwrap();
        let html = r#"<a id="downloadButton" href="https://download.mediafire.com/movie.rar">Download</a>"#;

        assert_eq!(
            mediafire_download_url(&page, html).unwrap().as_str(),
            "https://download.mediafire.com/movie.rar"
        );
        assert!(is_mediafire_page(page.as_str()));
    }

    #[test]
    fn reliable_sources_are_sorted_first() {
        assert!(
            source_preference("https://www.mediafire.com/file/a")
                < source_preference("https://mega.nz/file/a")
        );
    }

    #[test]
    fn archive_sources_are_labeled_before_download() {
        let rar = Url::parse("https://mediafire.com/file/movie.rar/file").unwrap();
        let video = Url::parse("https://cdn.example.com/movie.mkv").unwrap();

        assert_eq!(
            source_format_label(&rar).as_deref(),
            Some("RAR - requiere extraer")
        );
        assert_eq!(source_format_label(&video).as_deref(), Some("Video MKV"));
    }

    #[test]
    fn download_concurrency_defaults_and_stays_bounded() {
        assert_eq!(normalize_concurrency(None), 1);
        assert_eq!(normalize_concurrency(Some(0)), 1);
        assert_eq!(normalize_concurrency(Some(4)), 4);
        assert_eq!(normalize_concurrency(Some(99)), 10);
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

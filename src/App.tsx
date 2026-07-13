import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { readText } from "@tauri-apps/plugin-clipboard-manager";
import { relaunch } from "@tauri-apps/plugin-process";
import { check, type Update } from "@tauri-apps/plugin-updater";
import {
  AlertCircle,
  CheckCircle2,
  ClipboardPaste,
  Download,
  FileDown,
  FolderInput,
  Link2,
  ListPlus,
  LogIn,
  Loader2,
  Plus,
  RefreshCw,
  Save,
  Search,
  Settings2,
  Square,
  Trash2,
  X,
  XCircle
} from "lucide-react";
import type { ReactNode } from "react";
import { useEffect, useMemo, useRef, useState } from "react";

type DownloadStatus =
  | "pending"
  | "extracting"
  | "downloading"
  | "processing"
  | "completed"
  | "cancelled"
  | "failed";

type ToolProbe = {
  available: boolean;
  path?: string;
  version?: string;
  message?: string;
};

type ToolState = {
  yt_dlp: ToolProbe;
  ffmpeg: ToolProbe;
  javascript: ToolProbe;
  default_output_dir?: string;
};

type ProbeEntry = {
  id?: string;
  title?: string;
  url: string;
  webpage_url?: string;
  duration?: string;
  kind?: string;
  source?: string;
  choice_group?: string;
  resolutions: number[];
};

type ProbeResult = {
  source_url: string;
  title?: string;
  entries: ProbeEntry[];
};

type QueueItem = {
  id: string;
  title: string;
  url: string;
  status: DownloadStatus;
  progress: number;
  speed?: string;
  eta?: string;
  file?: string;
  message?: string;
  resolutions: number[];
  resolution: number | null;
  referer?: string;
};

type FoundEntry = ProbeEntry & {
  selected: boolean;
  resolution: number | null;
  groupId: string;
  contentTitle: string;
  sourcePage: string;
};

type FoundGroup = {
  id: string;
  title: string;
  sourcePage: string;
  entries: FoundEntry[];
};

type DetectionFailure = {
  url: string;
  error: string;
};

type DownloadEvent = {
  id: string;
  status: DownloadStatus;
  progress?: number;
  speed?: string;
  eta?: string;
  file?: string;
  message?: string;
};

type BatchStateEvent = {
  active: boolean;
  message?: string;
};

type DownloadSnapshot = {
  active: boolean;
  updates: DownloadEvent[];
};

type YoutubeSessionState = {
  active: boolean;
  cookie_count: number;
  message: string;
};

type QueueConfirmation = {
  action: "stop" | "delete";
  item: QueueItem;
};

type UpdateState = {
  status: "idle" | "checking" | "available" | "downloading" | "current" | "error";
  version?: string;
  progress: number;
  message: string;
};

const statusCopy: Record<DownloadStatus, string> = {
  pending: "Pendiente",
  extracting: "Extrayendo",
  downloading: "Descargando",
  processing: "Procesando",
  completed: "Completo",
  cancelled: "Detenida",
  failed: "Fallo"
};

const statusIcon: Record<DownloadStatus, ReactNode> = {
  pending: <FileDown size={16} />,
  extracting: <Loader2 size={16} className="spin" />,
  downloading: <Download size={16} />,
  processing: <RefreshCw size={16} className="spin" />,
  completed: <CheckCircle2 size={16} />,
  cancelled: <Square size={15} />,
  failed: <AlertCircle size={16} />
};

function isActiveStatus(status: DownloadStatus) {
  return ["extracting", "downloading", "processing"].includes(status);
}

function mergeDownloadUpdate(item: QueueItem, payload: DownloadEvent) {
  if (item.id !== payload.id) return item;

  return {
    ...item,
    status: payload.status,
    progress:
      payload.progress ??
      (payload.status === "completed" ? 100 : item.progress),
    speed: payload.speed ?? item.speed,
    eta: payload.eta ?? item.eta,
    file: payload.file ?? item.file,
    message: payload.message ?? item.message
  };
}

function makeId() {
  return crypto.randomUUID();
}

function extractUrls(raw: string): string[] {
  return [...(raw.match(/https?:\/\/[^\s,]+/gi) ?? [])];
}

function entryTitle(entry: ProbeEntry) {
  return entry.title?.trim() || entry.webpage_url || entry.url;
}

function compactUrl(url: string) {
  try {
    const parsed = new URL(url);
    const tail = parsed.pathname.replace(/\/$/, "").split("/").pop();
    return tail ? `${parsed.hostname}/${tail}` : parsed.hostname;
  } catch {
    return url;
  }
}

function isYoutubeUrl(raw: string) {
  try {
    const host = new URL(raw).hostname.toLowerCase();
    return (
      host === "youtu.be" ||
      host.endsWith(".youtu.be") ||
      host === "youtube.com" ||
      host.endsWith(".youtube.com")
    );
  } catch {
    return false;
  }
}

export function App() {
  const sourceInputRef = useRef<HTMLTextAreaElement>(null);
  const availableUpdate = useRef<Update | null>(null);
  const lastLoggedStatus = useRef<Map<string, DownloadStatus>>(new Map());
  const [sourceText, setSourceText] = useState("");
  const [referer, setReferer] = useState("");
  const [browser, setBrowser] = useState("app");
  const [youtubeSession, setYoutubeSession] =
    useState<YoutubeSessionState | null>(null);
  const [sessionBusy, setSessionBusy] = useState(false);
  const [outputDir, setOutputDir] = useState("");
  const [tools, setTools] = useState<ToolState | null>(null);
  const [found, setFound] = useState<FoundEntry[]>([]);
  const [detectionFailures, setDetectionFailures] = useState<DetectionFailure[]>([]);
  const [retryingSources, setRetryingSources] = useState<Set<string>>(new Set());
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [logs, setLogs] = useState<string[]>([]);
  const [isProbing, setIsProbing] = useState(false);
  const [downloadConcurrency, setDownloadConcurrency] = useState(() => {
    const stored = Number(localStorage.getItem("descargador-a1-concurrency"));
    return [1, 2, 3, 4, 5, 10].includes(stored) ? stored : 1;
  });
  const [isRunning, setIsRunning] = useState(false);
  const [notice, setNotice] = useState("");
  const [confirmation, setConfirmation] = useState<QueueConfirmation | null>(
    null
  );
  const [isConfirming, setIsConfirming] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [appVersion, setAppVersion] = useState("1.1.1");
  const [updateState, setUpdateState] = useState<UpdateState>({
    status: "idle",
    progress: 0,
    message: "Busca nuevas versiones publicadas en GitHub."
  });
  const [uiScale, setUiScale] = useState(() => {
    const stored = Number(localStorage.getItem("descargador-a1-ui-scale"));
    return stored >= 75 && stored <= 125 ? stored : 100;
  });

  const selectedFound = useMemo(
    () => found.filter((entry) => entry.selected),
    [found]
  );

  const foundGroups = useMemo<FoundGroup[]>(() => {
    const groups = new Map<string, FoundGroup>();
    for (const entry of found) {
      const existing = groups.get(entry.groupId);
      if (existing) {
        existing.entries.push(entry);
      } else {
        groups.set(entry.groupId, {
          id: entry.groupId,
          title: entry.contentTitle,
          sourcePage: entry.sourcePage,
          entries: [entry]
        });
      }
    }
    return [...groups.values()];
  }, [found]);

  const queueStats = useMemo(() => {
    const total = queue.length;
    const done = queue.filter((item) => item.status === "completed").length;
    const failed = queue.filter((item) => item.status === "failed").length;
    const active = queue.filter((item) => isActiveStatus(item.status)).length;
    const stopped = queue.filter((item) => item.status === "cancelled").length;
    return { total, done, failed, active, stopped };
  }, [queue]);

  useEffect(() => {
    refreshTools();
    refreshYoutubeSession();
    getVersion().then(setAppVersion).catch(() => undefined);

    const updateTimer = window.setTimeout(() => {
      void checkForUpdates(true);
    }, 1800);

    let unlistenJob: (() => void) | undefined;
    let unlistenBatch: (() => void) | undefined;
    let polling = false;

    listen<DownloadEvent>("download://job-update", (event) => {
      const payload = event.payload;
      setQueue((current) =>
        current.map((item) => mergeDownloadUpdate(item, payload))
      );

      const previousStatus = lastLoggedStatus.current.get(payload.id);
      if (payload.message && previousStatus !== payload.status) {
        lastLoggedStatus.current.set(payload.id, payload.status);
        setLogs((current) =>
          [`${statusCopy[payload.status]}: ${payload.message}`, ...current].slice(
            0,
            140
          )
        );
      }
    }).then((unlisten) => {
      unlistenJob = unlisten;
    });

    listen<BatchStateEvent>("download://batch-state", (event) => {
      setIsRunning(event.payload.active);
      if (event.payload.message) setNotice(event.payload.message);
    }).then((unlisten) => {
      unlistenBatch = unlisten;
    });

    async function pollDownloadState() {
      if (polling) return;
      polling = true;
      try {
        const snapshot = await invoke<DownloadSnapshot>("get_download_snapshot");
        setIsRunning(snapshot.active);
        setQueue((current) =>
          current.map((item) => {
            const update = snapshot.updates.find((entry) => entry.id === item.id);
            return update ? mergeDownloadUpdate(item, update) : item;
          })
        );
      } catch {
        // Event updates remain the primary path while the backend is restarting.
      } finally {
        polling = false;
      }
    }

    void pollDownloadState();
    const pollTimer = window.setInterval(pollDownloadState, 750);

    return () => {
      window.clearTimeout(updateTimer);
      window.clearInterval(pollTimer);
      unlistenJob?.();
      unlistenBatch?.();
    };
  }, []);

  async function checkForUpdates(silent = false) {
    setUpdateState((current) => ({
      ...current,
      status: "checking",
      progress: 0,
      message: "Buscando actualizaciones..."
    }));
    try {
      const update = await check({ timeout: 15000 });
      availableUpdate.current = update;
      if (!update) {
        setUpdateState({
          status: "current",
          progress: 100,
          message: `Descargador A1 v${appVersion} esta actualizado.`
        });
        return;
      }

      setUpdateState({
        status: "available",
        version: update.version,
        progress: 0,
        message: update.body || `Version ${update.version} disponible.`
      });
      if (silent) {
        setNotice(`Actualizacion ${update.version} disponible.`);
      }
    } catch (error) {
      availableUpdate.current = null;
      setUpdateState({
        status: "error",
        progress: 0,
        message: `No pude consultar actualizaciones: ${String(error)}`
      });
    }
  }

  async function installAvailableUpdate() {
    let update = availableUpdate.current;
    if (!update) {
      await checkForUpdates();
      update = availableUpdate.current;
    }
    if (!update) return;

    let downloaded = 0;
    let total = 0;
    setUpdateState((current) => ({
      ...current,
      status: "downloading",
      progress: 0,
      message: `Descargando version ${update.version}...`
    }));
    try {
      await update.downloadAndInstall((event) => {
        if (event.event === "Started") {
          total = event.data.contentLength ?? 0;
        } else if (event.event === "Progress") {
          downloaded += event.data.chunkLength;
          const progress = total > 0 ? (downloaded / total) * 100 : 0;
          setUpdateState((current) => ({ ...current, progress }));
        } else if (event.event === "Finished") {
          setUpdateState((current) => ({
            ...current,
            progress: 100,
            message: "Actualizacion instalada. Reiniciando..."
          }));
        }
      });
      await relaunch();
    } catch (error) {
      setUpdateState({
        status: "error",
        version: update.version,
        progress: 0,
        message: `No pude instalar la actualizacion: ${String(error)}`
      });
    }
  }

  useEffect(() => {
    document.documentElement.style.setProperty(
      "--ui-scale",
      String(uiScale / 100)
    );
    localStorage.setItem("descargador-a1-ui-scale", String(uiScale));
  }, [uiScale]);

  useEffect(() => {
    localStorage.setItem(
      "descargador-a1-concurrency",
      String(downloadConcurrency)
    );
  }, [downloadConcurrency]);

  useEffect(() => {
    if (!confirmation) return;

    function closeOnEscape(event: KeyboardEvent) {
      if (event.key === "Escape" && !isConfirming) setConfirmation(null);
    }

    document.addEventListener("keydown", closeOnEscape);
    return () => document.removeEventListener("keydown", closeOnEscape);
  }, [confirmation, isConfirming]);

  useEffect(() => {
    if (!settingsOpen) return;

    function closeSettings(event: KeyboardEvent) {
      if (event.key === "Escape") setSettingsOpen(false);
    }

    document.addEventListener("keydown", closeSettings);
    return () => document.removeEventListener("keydown", closeSettings);
  }, [settingsOpen]);

  async function refreshTools() {
    try {
      const state = await invoke<ToolState>("check_tools");
      setTools(state);
      if (state.default_output_dir && !outputDir) {
        setOutputDir(state.default_output_dir);
      }
    } catch (error) {
      setNotice(String(error));
    }
  }

  async function refreshYoutubeSession() {
    try {
      setYoutubeSession(
        await invoke<YoutubeSessionState>("get_youtube_session")
      );
    } catch {
      setYoutubeSession(null);
    }
  }

  async function openYoutubeLogin() {
    setSessionBusy(true);
    try {
      await invoke("open_youtube_login");
      setNotice("Se abrio el inicio de sesion de YouTube.");
    } catch (error) {
      setNotice(String(error));
    } finally {
      setSessionBusy(false);
    }
  }

  async function saveYoutubeSession() {
    setSessionBusy(true);
    try {
      const state = await invoke<YoutubeSessionState>("save_youtube_session");
      setYoutubeSession(state);
      setBrowser("app");
      setNotice(state.message);
    } catch (error) {
      setNotice(String(error));
    } finally {
      setSessionBusy(false);
    }
  }

  async function probeSingleUrl(url: string) {
    try {
      return await invoke<ProbeResult>("probe_url", {
        url,
        browser: browser === "none" ? null : browser
      });
    } catch (extractorError) {
      if (String(extractorError).includes("Busqueda detenida")) {
        throw extractorError;
      }
      if (isYoutubeUrl(url)) {
        throw new Error(`YouTube: ${String(extractorError)}`);
      }
      try {
        const result = await invoke<ProbeResult>("scan_page", {
          url,
          referer: referer.trim() || null
        });
        return result;
      } catch (scanError) {
        if (String(scanError).includes("Busqueda detenida")) {
          throw scanError;
        }
        throw new Error(
          `yt-dlp: ${String(extractorError)} | Escaneo: ${String(scanError)}`
        );
      }
    }
  }

  function mapProbeEntries(result: ProbeResult, sourceUrl: string) {
    const mapped: FoundEntry[] = [];
    const selectedGroups = new Set<string>();
    const seen = new Set<string>();
    for (const entry of result.entries) {
      const key = entry.webpage_url || entry.url;
      if (seen.has(key)) continue;
      seen.add(key);
      const choiceGroup = entry.choice_group;
      const selected = !choiceGroup || !selectedGroups.has(choiceGroup);
      if (choiceGroup && selected) selectedGroups.add(choiceGroup);
      const groupId = choiceGroup || key;
      const contentTitle = choiceGroup
        ? result.title?.trim() || compactUrl(sourceUrl)
        : entry.title?.trim() || result.title?.trim() || compactUrl(sourceUrl);
      mapped.push({
        ...entry,
        resolutions: entry.resolutions ?? [],
        selected,
        resolution: null,
        groupId,
        contentTitle,
        sourcePage: result.source_url || sourceUrl
      });
    }
    return mapped;
  }

  async function probeSource() {
    const extracted = extractUrls(sourceText);
    const urls = extracted.length ? extracted : [sourceText.trim()];
    if (!urls[0]) {
      setNotice("Pega una URL para buscar.");
      return;
    }

    setIsProbing(true);
    setNotice("");
    try {
      const detected: FoundEntry[] = [];
      const seen = new Set<string>();
      const errors: DetectionFailure[] = [];

      for (const [urlIndex, url] of urls.entries()) {
        try {
          const result = await probeSingleUrl(url);
          for (const entry of mapProbeEntries(result, url)) {
            const key = entry.webpage_url || entry.url;
            if (seen.has(key)) continue;
            seen.add(key);
            detected.push(entry);
          }
          setNotice(`Buscando ${urlIndex + 1} de ${urls.length}...`);
        } catch (error) {
          if (String(error).includes("Busqueda detenida")) throw error;
          errors.push({ url, error: String(error) });
        }
      }

      setFound(detected);
      setDetectionFailures(errors);
      setNotice(
        `${new Set(detected.map((entry) => entry.groupId)).size} contenido(s) listo(s)` +
          (errors.length ? ` - ${errors.length} enlace(s) con error.` : ".")
      );
    } catch (error) {
      if (String(error).includes("Busqueda detenida")) {
        setNotice("Busqueda detenida.");
      } else {
        setNotice(String(error));
      }
    } finally {
      setIsProbing(false);
    }
  }

  async function retryFailedSource(failure: DetectionFailure) {
    setRetryingSources((current) => new Set(current).add(failure.url));
    try {
      const result = await probeSingleUrl(failure.url);
      const recovered = mapProbeEntries(result, failure.url);
      setFound((current) => {
        const existing = new Set(current.map((entry) => entry.webpage_url || entry.url));
        return [
          ...current,
          ...recovered.filter((entry) => !existing.has(entry.webpage_url || entry.url))
        ];
      });
      setDetectionFailures((current) =>
        current.filter((entry) => entry.url !== failure.url)
      );
      setNotice(`Enlace recuperado: ${compactUrl(failure.url)}.`);
    } catch (error) {
      setDetectionFailures((current) =>
        current.map((entry) =>
          entry.url === failure.url ? { ...entry, error: String(error) } : entry
        )
      );
      setNotice(`El enlace sigue fallando: ${compactUrl(failure.url)}.`);
    } finally {
      setRetryingSources((current) => {
        const next = new Set(current);
        next.delete(failure.url);
        return next;
      });
    }
  }

  async function stopProbe() {
    setNotice("Deteniendo busqueda...");
    try {
      await invoke("cancel_search");
    } catch (error) {
      setNotice(String(error));
    }
  }

  function clearSource() {
    setSourceText("");
    window.requestAnimationFrame(() => sourceInputRef.current?.focus());
  }

  async function pasteSources() {
    try {
      const clipboard = await readText();
      const pastedUrls = extractUrls(clipboard);
      if (!pastedUrls.length) {
        setNotice("El portapapeles no contiene enlaces.");
        return;
      }

      const urls = extractUrls(sourceText);
      const seen = new Set(urls);
      let added = 0;
      for (const url of pastedUrls) {
        if (!seen.has(url)) {
          seen.add(url);
          urls.push(url);
          added += 1;
        }
      }
      setSourceText(urls.join("\n"));
      window.requestAnimationFrame(() => sourceInputRef.current?.focus());
      setNotice(
        added > 0
          ? `${added} enlace(s) pegado(s). Podes seguir pegando o buscar todos.`
          : "Esos enlaces ya estaban en la lista."
      );
    } catch (error) {
      setNotice(`No pude leer el portapapeles: ${String(error)}`);
    }
  }

  function addUrlsFromText() {
    const urls = extractUrls(sourceText);
    if (!urls.length) {
      setNotice("No encontre URLs en el texto.");
      return;
    }

    addToQueue(
      urls.map((url) => ({
        url,
        title: compactUrl(url),
        referer: url
      }))
    );
  }

  function addSelectedFound() {
    if (!selectedFound.length) {
      setNotice("Selecciona al menos una fuente para agregar.");
      return;
    }
    addToQueue(foundEntriesToQueueInput(selectedFound));
  }

  async function downloadSelectedFound() {
    await downloadFoundEntries(selectedFound);
  }

  function entriesForGroup(groupId: string) {
    return found.filter((entry) => entry.groupId === groupId && entry.selected);
  }

  function addFoundGroup(groupId: string) {
    const entries = entriesForGroup(groupId);
    if (!entries.length) {
      setNotice("Selecciona una fuente para este contenido.");
      return;
    }
    addToQueue(foundEntriesToQueueInput(entries));
  }

  async function downloadFoundGroup(groupId: string) {
    const entries = entriesForGroup(groupId);
    if (!entries.length) {
      setNotice("Selecciona una fuente para descargar este contenido.");
      return;
    }
    await downloadFoundEntries(entries);
  }

  async function downloadFoundEntries(entries: FoundEntry[]) {
    if (!entries.length) {
      setNotice("Selecciona al menos una fuente para descargar.");
      return;
    }
    const inputs = foundEntriesToQueueInput(entries);
    const fresh = addToQueue(inputs);
    const selectedUrls = new Set(inputs.map((item) => item.url));
    const alreadyQueued = queue.filter(
      (item) =>
        selectedUrls.has(item.url) &&
        (item.status === "pending" || item.status === "failed")
    );

    await startJobs([...alreadyQueued, ...fresh]);
  }

  function removeFoundGroup(groupId: string) {
    setFound((current) => current.filter((entry) => entry.groupId !== groupId));
  }

  function foundEntriesToQueueInput(entries: FoundEntry[]) {
    return entries.map((entry) => ({
      url: entry.webpage_url || entry.url,
      title: entry.contentTitle || entryTitle(entry),
      resolutions: entry.resolutions,
      resolution: entry.resolution,
      referer: entry.sourcePage
    }));
  }

  function addToQueue(
    items: Array<{
      url: string;
      title: string;
      resolutions?: number[];
      resolution?: number | null;
      referer?: string;
    }>
  ) {
    const normalized = items.filter((item) => item.url.trim());
    if (!normalized.length) return [];

    const existing = new Set(queue.map((item) => item.url));
    const fresh = normalized
      .filter((item) => !existing.has(item.url))
      .map<QueueItem>((item) => ({
        id: makeId(),
        title: item.title,
        url: item.url,
        status: "pending",
        progress: 0,
        resolutions: item.resolutions ?? [],
        resolution: item.resolution ?? null,
        referer: item.referer
      }));

    if (!fresh.length) {
      setNotice("Esas URLs ya estan en la cola.");
      return [];
    }

    setQueue((current) => [...current, ...fresh]);
    setNotice(`${fresh.length} entrada(s) agregada(s) a la cola.`);
    return fresh;
  }

  async function startQueue() {
    const items = queue.filter(
      (item) =>
        item.status === "pending" ||
        item.status === "failed" ||
        item.status === "cancelled"
    );

    if (!items.length) {
      setNotice("No hay elementos pendientes.");
      return;
    }

    await startJobs(items);
  }

  async function startJobs(items: QueueItem[]) {
    if (!items.length) return;

    const jobs = items.map((item) => ({
      id: item.id,
      title: item.title,
      url: item.url,
      maxHeight: item.resolution,
      referer: item.referer
    }));
    const jobIds = new Set(jobs.map((job) => job.id));

    setQueue((current) =>
      current.map((item) =>
        jobIds.has(item.id)
          ? {
              ...item,
              status: "extracting",
              progress: 0,
              speed: undefined,
              eta: undefined,
              message: "Preparando descarga..."
            }
          : item
      )
    );

    try {
      await invoke("start_download", {
        jobs,
        outputDir: outputDir.trim() || null,
        referer: referer.trim() || null,
        browser: browser === "none" ? null : browser,
        concurrency: downloadConcurrency
      });
      setIsRunning(true);
    } catch (error) {
      setQueue((current) =>
        current.map((item) =>
          jobIds.has(item.id)
            ? {
                ...item,
                status: "failed",
                message: String(error)
              }
            : item
        )
      );
      setNotice(String(error));
    }
  }

  function clearFinished() {
    setQueue((current) =>
      current.filter((item) => item.status !== "completed")
    );
  }

  function clearAll() {
    if (isRunning) return;
    setQueue([]);
    setLogs([]);
    setFound([]);
    setDetectionFailures([]);
  }

  async function confirmQueueAction() {
    if (!confirmation) return;

    const { action, item } = confirmation;
    const currentItem = queue.find((entry) => entry.id === item.id) ?? item;
    setIsConfirming(true);

    try {
      const shouldCancel = action === "stop" || isActiveStatus(currentItem.status);
      const cancelled = shouldCancel
        ? await invoke<boolean>("cancel_download", { id: item.id })
        : false;

      if (action === "delete") {
        setQueue((current) => current.filter((entry) => entry.id !== item.id));
        setNotice(
          cancelled
            ? "Descarga detenida y eliminada de la lista."
            : "Descarga eliminada de la lista."
        );
      } else if (cancelled) {
        setQueue((current) =>
          current.map((entry) =>
            entry.id === item.id
              ? {
                  ...entry,
                  status: "cancelled",
                  speed: undefined,
                  eta: undefined,
                  message: "Descarga detenida."
                }
              : entry
          )
        );
        setNotice("Descarga detenida.");
      } else {
        setNotice("La descarga ya habia terminado.");
      }

      setConfirmation(null);
    } catch (error) {
      setNotice(`No pude completar la accion: ${String(error)}`);
    } finally {
      setIsConfirming(false);
    }
  }

  const ytReady = Boolean(
    tools?.yt_dlp.available &&
      tools?.ffmpeg.available &&
      tools?.javascript.available
  );

  return (
    <main className="app-shell">
      <header className="topbar">
        <div className="brand">
          <div className="brand-mark">
            <Download size={20} />
          </div>
          <div>
            <h1>Descargador A1</h1>
            <p>{queueStats.total} en cola</p>
          </div>
        </div>

        <div className="tool-strip">
          <ToolBadge label="yt-dlp" probe={tools?.yt_dlp} required />
          <ToolBadge label="ffmpeg" probe={tools?.ffmpeg} required />
          <ToolBadge label="Node" probe={tools?.javascript} required />
          <button
            className="icon-button"
            onClick={() => setSettingsOpen(true)}
            title="Configuracion"
          >
            <Settings2 size={17} />
          </button>
          <button
            className="icon-button"
            onClick={refreshTools}
            title="Actualizar herramientas"
          >
            <RefreshCw size={17} />
          </button>
        </div>
      </header>

      <section className="workspace">
        <section className="control-panel">
          <div className="panel-head">
            <h2>Entrada</h2>
            <Link2 size={18} />
          </div>

          <div className="field">
            <div className="field-label-row">
              <span>URL o lista</span>
              <div className="field-label-actions">
                <button
                  className="paste-button"
                  onClick={() => void pasteSources()}
                  title="Pegar enlaces del portapapeles"
                >
                  <ClipboardPaste size={15} />
                  Pegar
                </button>
                <button
                  className="clear-input-button"
                  onClick={clearSource}
                  disabled={!sourceText}
                  title="Limpiar URL"
                  aria-label="Limpiar URL"
                >
                  <X size={15} />
                </button>
              </div>
            </div>
            <textarea
              ref={sourceInputRef}
              aria-label="URL o lista"
              value={sourceText}
              onChange={(event) => setSourceText(event.target.value)}
              placeholder="https://..."
              rows={7}
            />
          </div>

          <div className="action-grid">
            <button
              className={isProbing ? "ghost-button stop-button" : "primary-button"}
              onClick={isProbing ? stopProbe : probeSource}
              title={isProbing ? "Detener busqueda" : "Buscar links"}
            >
              {isProbing ? (
                <Square size={16} />
              ) : (
                <Search size={17} />
              )}
              {isProbing ? "Detener busqueda" : "Buscar"}
            </button>
              <button
                className="ghost-button"
                onClick={addUrlsFromText}
                disabled={!sourceText.trim()}
                title="Agregar URLs a la cola"
              >
                <ListPlus size={17} />
                Agregar a cola
              </button>
          </div>

          <label className="field">
            <span>Referer manual (opcional)</span>
            <input
              value={referer}
              onChange={(event) => setReferer(event.target.value)}
              placeholder="https://dominio-autorizado.com/"
            />
          </label>

          <div className="field session-field">
            <span>Sesion de YouTube</span>
            <div className="session-controls">
              <select
                value={browser}
                onChange={(event) => setBrowser(event.target.value)}
                aria-label="Sesion de YouTube"
              >
                <option value="app">Automatica</option>
                <option value="none">Sin sesion</option>
                <option value="edge">Microsoft Edge</option>
                <option value="chrome">Google Chrome</option>
                <option value="firefox">Mozilla Firefox</option>
                <option value="brave">Brave</option>
              </select>
              <button
                className="ghost-button compact-button"
                onClick={openYoutubeLogin}
                disabled={sessionBusy}
              >
                <LogIn size={17} />
                Iniciar sesion
              </button>
              <button
                className="ghost-button compact-button"
                onClick={saveYoutubeSession}
                disabled={sessionBusy}
              >
                <Save size={17} />
                Usar sesion
              </button>
            </div>
            <small className={youtubeSession?.active ? "session-ready" : "muted"}>
              {youtubeSession?.message ?? "Sin sesion de YouTube."}
            </small>
          </div>

          <label className="field">
            <span>Carpeta</span>
            <div className="input-icon">
              <FolderInput size={17} />
              <input
                value={outputDir}
                onChange={(event) => setOutputDir(event.target.value)}
                placeholder="Downloads\\Descargador A1"
              />
            </div>
          </label>
        </section>

        {(found.length > 0 || detectionFailures.length > 0) && (
          <section className="found-panel">
            <div className="panel-head compact">
              <div>
                <h2>Detectados</h2>
                <p className="found-count">
                  {foundGroups.length} listo(s)
                  {detectionFailures.length > 0
                    ? ` - ${detectionFailures.length} con error`
                    : ""}
                </p>
              </div>
              <div className="mini-actions">
                <button
                  className="ghost-button compact-button"
                  onClick={addSelectedFound}
                  title="Agregar seleccionados"
                >
                  <Plus size={17} />
                  Agregar todos
                </button>
                <button
                  className="primary-button compact-button"
                  onClick={downloadSelectedFound}
                  disabled={isRunning}
                  title="Descargar seleccionados"
                >
                  <Download size={17} />
                  Descargar todo
                </button>
              </div>
            </div>

            <div className="found-list">
              {foundGroups.map((group) => {
                const selectedInGroup = group.entries.filter(
                  (entry) => entry.selected
                ).length;
                return (
                  <section className="found-group" key={group.id}>
                    <div className="found-group-head">
                      <div className="found-group-title">
                        <h3>{group.title}</h3>
                        <small>{compactUrl(group.sourcePage)}</small>
                        <span>
                          {selectedInGroup} de {group.entries.length} fuente(s) elegida(s)
                        </span>
                      </div>
                      <div className="found-group-actions">
                        <button
                          className="ghost-button compact-button"
                          onClick={() => addFoundGroup(group.id)}
                          title="Agregar este contenido a la cola"
                        >
                          <Plus size={16} />
                          Agregar
                        </button>
                        <button
                          className="primary-button compact-button"
                          onClick={() => void downloadFoundGroup(group.id)}
                          disabled={isRunning}
                          title="Descargar este contenido"
                        >
                          <Download size={16} />
                          Descargar
                        </button>
                        <button
                          className="icon-button danger compact-icon-button"
                          onClick={() => removeFoundGroup(group.id)}
                          title="Eliminar este contenido"
                          aria-label={`Eliminar ${group.title}`}
                        >
                          <Trash2 size={16} />
                        </button>
                      </div>
                    </div>

                    <div className="found-options">
                      {group.entries.map((entry) => (
                        <div className="found-row" key={entry.url}>
                          <input
                            type="checkbox"
                            aria-label={`Seleccionar ${entryTitle(entry)}`}
                            checked={entry.selected}
                            onChange={(event) => {
                              const checked = event.target.checked;
                              setFound((current) =>
                                current.map((item) =>
                                  item.groupId === entry.groupId && item.url === entry.url
                                    ? { ...item, selected: checked }
                                    : checked && item.groupId === entry.groupId
                                      ? { ...item, selected: false }
                                      : item
                                )
                              );
                            }}
                          />
                          <span className="found-details">
                            <strong>{entryTitle(entry)}</strong>
                            <small className="link-line">
                              <span className="link-kind">{entry.kind || "link"}</span>
                              {compactUrl(entry.webpage_url || entry.url)}
                            </small>
                          </span>
                          <div className="found-actions">
                            {entry.resolutions.length > 0 && (
                              <select
                                className="resolution-select"
                                value={entry.resolution ?? "best"}
                                onChange={(event) => {
                                  const value = event.target.value;
                                  setFound((current) =>
                                    current.map((item) =>
                                      item.groupId === entry.groupId && item.url === entry.url
                                        ? {
                                            ...item,
                                            resolution:
                                              value === "best" ? null : Number(value)
                                          }
                                        : item
                                    )
                                  );
                                }}
                                title="Resolucion de descarga"
                              >
                                <option value="best">Mejor disponible</option>
                                {entry.resolutions.map((height) => (
                                  <option value={height} key={height}>
                                    {height}p
                                  </option>
                                ))}
                              </select>
                            )}
                            <button
                              className="icon-button danger compact-icon-button"
                              onClick={() =>
                                setFound((current) =>
                                  current.filter(
                                    (item) =>
                                      item.groupId !== entry.groupId || item.url !== entry.url
                                  )
                                )
                              }
                              title="Eliminar fuente"
                              aria-label={`Eliminar ${entryTitle(entry)}`}
                            >
                              <Trash2 size={16} />
                            </button>
                          </div>
                        </div>
                      ))}
                    </div>
                  </section>
                );
              })}
              {detectionFailures.map((failure) => {
                const retrying = retryingSources.has(failure.url);
                return (
                  <section className="found-group detection-failure" key={failure.url}>
                    <div className="found-group-head">
                      <div className="found-group-title">
                        <h3>{compactUrl(failure.url)}</h3>
                        <small>{failure.url}</small>
                        <span>No se pudo detectar. Los demas continuan normalmente.</span>
                      </div>
                      <div className="found-group-actions">
                        <button
                          className="ghost-button compact-button"
                          onClick={() => void retryFailedSource(failure)}
                          disabled={retrying || isProbing}
                          title="Volver a detectar este enlace"
                        >
                          <RefreshCw size={16} className={retrying ? "spin" : undefined} />
                          Reintentar
                        </button>
                        <button
                          className="icon-button danger compact-icon-button"
                          onClick={() =>
                            setDetectionFailures((current) =>
                              current.filter((entry) => entry.url !== failure.url)
                            )
                          }
                          title="Quitar enlace fallido"
                          aria-label={`Quitar ${failure.url}`}
                        >
                          <Trash2 size={16} />
                        </button>
                      </div>
                    </div>
                    <p className="failure-message">{failure.error}</p>
                  </section>
                );
              })}
            </div>
          </section>
        )}

        <section className="download-panel">
          <div className="panel-head queue-head">
            <div>
              <h2>Descargas</h2>
              <p>
                {queueStats.done} completas - {queueStats.active} activas -{" "}
                {queueStats.stopped} detenidas - {queueStats.failed} con error
              </p>
            </div>

            <div className="toolbar">
              <label className="concurrency-control">
                <span>Simultaneas</span>
                <select
                  value={downloadConcurrency}
                  onChange={(event) =>
                    setDownloadConcurrency(Number(event.target.value))
                  }
                  disabled={isRunning}
                >
                  {[1, 2, 3, 4, 5, 10].map((value) => (
                    <option value={value} key={value}>
                      {value}
                    </option>
                  ))}
                </select>
              </label>
              <button
                className="primary-button"
                onClick={startQueue}
                disabled={isRunning || !queue.length || !ytReady}
                title={
                  ytReady
                    ? "Descargar elementos pendientes"
                    : "yt-dlp no esta disponible"
                }
              >
                <Download size={17} />
                Descargar cola
              </button>
              <button
                className="icon-button"
                onClick={clearFinished}
                disabled={!queue.some((item) => item.status === "completed")}
                title="Quitar completas"
              >
                <CheckCircle2 size={17} />
              </button>
              <button
                className="icon-button danger"
                onClick={clearAll}
                disabled={isRunning || !queue.length}
                title="Vaciar cola"
              >
                <Trash2 size={17} />
              </button>
            </div>
          </div>

          {notice && (
            <div className="notice">
              <AlertCircle size={16} />
              <span>{notice}</span>
            </div>
          )}

          <div className="queue-list">
            {queue.length === 0 ? (
              <div className="empty-state">
                <FileDown size={34} />
                <p>Sin elementos en cola</p>
              </div>
            ) : (
              queue.map((item) => (
                <QueueRow
                  item={item}
                  canDownload={ytReady && !isRunning}
                  onDownload={() => startJobs([item])}
                  onStop={() => setConfirmation({ action: "stop", item })}
                  onDelete={() => setConfirmation({ action: "delete", item })}
                  onResolutionChange={(resolution) =>
                    setQueue((current) =>
                      current.map((entry) =>
                        entry.id === item.id ? { ...entry, resolution } : entry
                      )
                    )
                  }
                  key={item.id}
                />
              ))
            )}
          </div>
        </section>

        <section className="log-panel">
          <div className="panel-head compact">
            <h2>Actividad</h2>
            <XCircle size={18} />
          </div>

          <div className="log-list">
            {logs.length === 0 ? (
              <span className="muted">Sin eventos todavia</span>
            ) : (
              logs.map((log, index) => <p key={`${log}-${index}`}>{log}</p>)
            )}
          </div>
        </section>
      </section>

      {settingsOpen && (
        <div
          className="modal-backdrop"
          onMouseDown={(event) => {
            if (event.currentTarget === event.target) setSettingsOpen(false);
          }}
        >
          <section
            className="confirm-dialog settings-dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="settings-title"
          >
            <button
              className="icon-button dialog-close"
              onClick={() => setSettingsOpen(false)}
              title="Cerrar"
            >
              <X size={18} />
            </button>
            <div className="dialog-icon settings-icon">
              <Settings2 size={22} />
            </div>
            <h2 id="settings-title">Configuracion</h2>

            <div className="update-control">
              <span className="setting-heading">
                <strong>Actualizaciones</strong>
                <output>v{appVersion}</output>
              </span>
              <p>{updateState.message}</p>
              {updateState.status === "downloading" && (
                <progress max="100" value={updateState.progress} />
              )}
              <button
                className={
                  updateState.status === "available"
                    ? "primary-button"
                    : "ghost-button"
                }
                onClick={() =>
                  updateState.status === "available"
                    ? void installAvailableUpdate()
                    : void checkForUpdates()
                }
                disabled={
                  updateState.status === "checking" ||
                  updateState.status === "downloading"
                }
              >
                {updateState.status === "checking" ||
                updateState.status === "downloading" ? (
                  <Loader2 size={17} className="spin" />
                ) : updateState.status === "available" ? (
                  <Download size={17} />
                ) : (
                  <RefreshCw size={17} />
                )}
                {updateState.status === "available"
                  ? `Instalar v${updateState.version}`
                  : updateState.status === "downloading"
                    ? "Instalando..."
                    : "Buscar actualizaciones"}
              </button>
            </div>

            <label className="scale-control">
              <span>
                <strong>Escala de interfaz</strong>
                <output>{uiScale}%</output>
              </span>
              <input
                type="range"
                min="75"
                max="125"
                step="5"
                value={uiScale}
                onInput={(event) =>
                  setUiScale(Number(event.currentTarget.value))
                }
              />
            </label>

            <div className="dialog-actions">
              <button className="ghost-button" onClick={() => setUiScale(100)}>
                Restablecer
              </button>
              <button
                className="primary-button"
                onClick={() => setSettingsOpen(false)}
              >
                Listo
              </button>
            </div>
          </section>
        </div>
      )}

      {confirmation && (
        <div
          className="modal-backdrop"
          onMouseDown={(event) => {
            if (event.currentTarget === event.target && !isConfirming) {
              setConfirmation(null);
            }
          }}
        >
          <section
            className="confirm-dialog"
            role="alertdialog"
            aria-modal="true"
            aria-labelledby="confirm-title"
            aria-describedby="confirm-description"
          >
            <button
              className="icon-button dialog-close"
              onClick={() => setConfirmation(null)}
              disabled={isConfirming}
              title="Cerrar"
              autoFocus
            >
              <X size={18} />
            </button>

            <div className="dialog-icon">
              {confirmation.action === "stop" ? (
                <Square size={22} />
              ) : (
                <Trash2 size={22} />
              )}
            </div>
            <h2 id="confirm-title">
              {confirmation.action === "stop"
                ? "Detener descarga"
                : "Eliminar descarga"}
            </h2>
            <p id="confirm-description">
              {confirmation.action === "stop"
                ? "La descarga se detendra y podras iniciarla nuevamente desde la lista."
                : isActiveStatus(confirmation.item.status)
                  ? "La descarga se detendra y se eliminara de la lista."
                  : "La descarga se eliminara de la lista."}
            </p>
            <strong title={confirmation.item.title}>
              {confirmation.item.title}
            </strong>

            <div className="dialog-actions">
              <button
                className="ghost-button"
                onClick={() => setConfirmation(null)}
                disabled={isConfirming}
              >
                Cancelar
              </button>
              <button
                className="primary-button danger-button"
                onClick={confirmQueueAction}
                disabled={isConfirming}
              >
                {isConfirming ? (
                  <Loader2 size={17} className="spin" />
                ) : confirmation.action === "stop" ? (
                  <Square size={16} />
                ) : (
                  <Trash2 size={17} />
                )}
                {confirmation.action === "stop" ? "Detener" : "Eliminar"}
              </button>
            </div>
          </section>
        </div>
      )}
    </main>
  );
}

function ToolBadge({
  label,
  probe,
  required = false
}: {
  label: string;
  probe?: ToolProbe;
  required?: boolean;
}) {
  const available = probe?.available ?? false;
  const title = probe?.path || probe?.message || label;

  return (
    <span
      className={`tool-badge ${available ? "ok" : required ? "missing" : "soft"}`}
      title={title}
    >
      {available ? <CheckCircle2 size={15} /> : <AlertCircle size={15} />}
      {label}
      {probe?.version && <small>{probe.version}</small>}
    </span>
  );
}

function QueueRow({
  item,
  canDownload,
  onDownload,
  onStop,
  onDelete,
  onResolutionChange
}: {
  item: QueueItem;
  canDownload: boolean;
  onDownload: () => void;
  onStop: () => void;
  onDelete: () => void;
  onResolutionChange: (resolution: number | null) => void;
}) {
  const isPending =
    item.status === "pending" ||
    item.status === "failed" ||
    item.status === "cancelled";
  const isActive = isActiveStatus(item.status);

  return (
    <article className={`queue-row ${item.status}`}>
      <div className="queue-main">
        <div className="queue-title">
          <span className="status-icon">{statusIcon[item.status]}</span>
          <div>
            <h3>{item.title}</h3>
            <p>{compactUrl(item.url)}</p>
          </div>
        </div>

        <div className="queue-actions">
          {item.resolutions.length > 0 && (
            <select
              className="resolution-select queue-resolution"
              value={item.resolution ?? "best"}
              onChange={(event) =>
                onResolutionChange(
                  event.target.value === "best"
                    ? null
                    : Number(event.target.value)
                )
              }
              disabled={!isPending}
              title="Resolucion de descarga"
            >
              <option value="best">Mejor</option>
              {item.resolutions.map((height) => (
                <option value={height} key={height}>
                  {height}p
                </option>
              ))}
            </select>
          )}
          {isPending && (
            <button
              className="primary-button compact-button row-download"
              onClick={onDownload}
              disabled={!canDownload}
              title="Descargar este elemento"
            >
              <Download size={16} />
              Descargar
            </button>
          )}
          {isActive && (
            <button
              className="ghost-button compact-button stop-button"
              onClick={onStop}
              title="Detener esta descarga"
            >
              <Square size={15} />
              Detener
            </button>
          )}
          <button
            className="icon-button danger compact-icon-button"
            onClick={onDelete}
            title="Eliminar esta descarga"
          >
            <Trash2 size={16} />
          </button>
          <span className={`status-pill ${item.status}`}>
            {statusCopy[item.status]}
          </span>
        </div>
      </div>

      <div className="progress-track">
        <div
          className="progress-bar"
          style={{ width: `${Math.max(0, Math.min(100, item.progress))}%` }}
        />
      </div>

      <div className="queue-meta">
        <span>{Math.round(item.progress)}%</span>
        <span>{item.speed || "0 B/s"}</span>
        <span>{item.eta ? `ETA ${item.eta}` : "ETA --"}</span>
      </div>

      {(item.file || item.message) && (
        <p className="queue-message">{item.file || item.message}</p>
      )}
    </article>
  );
}

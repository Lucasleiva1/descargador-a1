import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  AlertCircle,
  CheckCircle2,
  Download,
  FileDown,
  FolderInput,
  Link2,
  ListPlus,
  Loader2,
  Plus,
  RefreshCw,
  Search,
  Trash2,
  XCircle
} from "lucide-react";
import type { ReactNode } from "react";
import { useEffect, useMemo, useState } from "react";

type DownloadStatus =
  | "pending"
  | "extracting"
  | "downloading"
  | "processing"
  | "completed"
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
};

type FoundEntry = ProbeEntry & {
  selected: boolean;
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

const statusCopy: Record<DownloadStatus, string> = {
  pending: "Pendiente",
  extracting: "Extrayendo",
  downloading: "Descargando",
  processing: "Procesando",
  completed: "Completo",
  failed: "Fallo"
};

const statusIcon: Record<DownloadStatus, ReactNode> = {
  pending: <FileDown size={16} />,
  extracting: <Loader2 size={16} className="spin" />,
  downloading: <Download size={16} />,
  processing: <RefreshCw size={16} className="spin" />,
  completed: <CheckCircle2 size={16} />,
  failed: <AlertCircle size={16} />
};

function makeId() {
  return crypto.randomUUID();
}

function extractUrls(raw: string) {
  return raw.match(/https?:\/\/[^\s,]+/gi) ?? [];
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

export function App() {
  const [sourceText, setSourceText] = useState("");
  const [referer, setReferer] = useState("");
  const [outputDir, setOutputDir] = useState("");
  const [tools, setTools] = useState<ToolState | null>(null);
  const [found, setFound] = useState<FoundEntry[]>([]);
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [logs, setLogs] = useState<string[]>([]);
  const [isProbing, setIsProbing] = useState(false);
  const [isRunning, setIsRunning] = useState(false);
  const [notice, setNotice] = useState("");

  const selectedFound = useMemo(
    () => found.filter((entry) => entry.selected),
    [found]
  );

  const queueStats = useMemo(() => {
    const total = queue.length;
    const done = queue.filter((item) => item.status === "completed").length;
    const failed = queue.filter((item) => item.status === "failed").length;
    const active = queue.filter((item) =>
      ["extracting", "downloading", "processing"].includes(item.status)
    ).length;
    return { total, done, failed, active };
  }, [queue]);

  useEffect(() => {
    refreshTools();

    let unlistenJob: (() => void) | undefined;
    let unlistenBatch: (() => void) | undefined;

    listen<DownloadEvent>("download://job-update", (event) => {
      const payload = event.payload;
      setQueue((current) =>
        current.map((item) => {
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
        })
      );

      if (payload.message) {
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

    return () => {
      unlistenJob?.();
      unlistenBatch?.();
    };
  }, []);

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

  async function probeSource() {
    const url = extractUrls(sourceText)[0] ?? sourceText.trim();
    if (!url) {
      setNotice("Pega una URL para buscar.");
      return;
    }

    setIsProbing(true);
    setNotice("");
    try {
      let result: ProbeResult;
      let mode = "yt-dlp";

      try {
        result = await invoke<ProbeResult>("probe_url", { url });
      } catch {
        mode = "escaneo";
        result = await invoke<ProbeResult>("scan_page", {
          url,
          referer: referer.trim() || null
        });
        if (!referer.trim()) {
          setReferer(url);
        }
      }

      setFound(result.entries.map((entry) => ({ ...entry, selected: true })));
      setNotice(
        result.entries.length === 1
          ? `Se encontro 1 entrada con ${mode}.`
          : `Se encontraron ${result.entries.length} entradas con ${mode}.`
      );
    } catch (error) {
      setNotice(String(error));
      setFound([]);
    } finally {
      setIsProbing(false);
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
        title: compactUrl(url)
      }))
    );
  }

  function addSelectedFound() {
    addToQueue(foundEntriesToQueueInput(selectedFound));
  }

  async function downloadSelectedFound() {
    const inputs = foundEntriesToQueueInput(selectedFound);
    const fresh = addToQueue(inputs);
    const selectedUrls = new Set(inputs.map((item) => item.url));
    const alreadyQueued = queue.filter(
      (item) =>
        selectedUrls.has(item.url) &&
        (item.status === "pending" || item.status === "failed")
    );

    await startJobs([...alreadyQueued, ...fresh]);
  }

  function foundEntriesToQueueInput(entries: FoundEntry[]) {
    return entries.map((entry) => ({
      url: entry.webpage_url || entry.url,
      title: entryTitle(entry)
    }));
  }

  function addToQueue(items: Array<{ url: string; title: string }>) {
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
        progress: 0
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
      (item) => item.status === "pending" || item.status === "failed"
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
      url: item.url
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
        referer: referer.trim() || null
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
  }

  const ytReady = tools?.yt_dlp.available ?? false;

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
          <ToolBadge label="ffmpeg" probe={tools?.ffmpeg} />
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

          <label className="field">
            <span>URL o lista</span>
            <textarea
              value={sourceText}
              onChange={(event) => setSourceText(event.target.value)}
              placeholder="https://..."
              rows={7}
            />
          </label>

          <div className="action-grid">
            <button
              className="primary-button"
              onClick={probeSource}
              disabled={isProbing}
              title="Buscar links"
            >
              {isProbing ? (
                <Loader2 size={17} className="spin" />
              ) : (
                <Search size={17} />
              )}
              Buscar
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
            <span>Referer</span>
            <input
              value={referer}
              onChange={(event) => setReferer(event.target.value)}
              placeholder="https://dominio-autorizado.com/"
            />
          </label>

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

        {found.length > 0 && (
          <section className="found-panel">
            <div className="panel-head compact">
              <div>
                <h2>Detectados</h2>
                <p className="found-count">{selectedFound.length} seleccionados</p>
              </div>
              <div className="mini-actions">
                <button
                  className="ghost-button compact-button"
                  onClick={addSelectedFound}
                  disabled={!selectedFound.length}
                  title="Agregar seleccionados"
                >
                  <Plus size={17} />
                  Agregar
                </button>
                <button
                  className="primary-button compact-button"
                  onClick={downloadSelectedFound}
                  disabled={!selectedFound.length || isRunning}
                  title="Descargar seleccionados"
                >
                  <Download size={17} />
                  Descargar
                </button>
              </div>
            </div>

            <div className="found-list">
              {found.map((entry, index) => (
                <label className="found-row" key={`${entry.url}-${index}`}>
                  <input
                    type="checkbox"
                    checked={entry.selected}
                    onChange={(event) => {
                      const checked = event.target.checked;
                      setFound((current) =>
                        current.map((item, itemIndex) =>
                          itemIndex === index
                            ? { ...item, selected: checked }
                            : item
                        )
                      );
                    }}
                  />
                  <span>
                    <strong>{entryTitle(entry)}</strong>
                    <small className="link-line">
                      <span className="link-kind">{entry.kind || "link"}</span>
                      {compactUrl(entry.webpage_url || entry.url)}
                    </small>
                  </span>
                </label>
              ))}
            </div>
          </section>
        )}

        <section className="download-panel">
          <div className="panel-head queue-head">
            <div>
              <h2>Descargas</h2>
              <p>
                {queueStats.done} completas - {queueStats.active} activas -{" "}
                {queueStats.failed} con error
              </p>
            </div>

            <div className="toolbar">
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
  onDownload
}: {
  item: QueueItem;
  canDownload: boolean;
  onDownload: () => void;
}) {
  const isPending = item.status === "pending" || item.status === "failed";

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

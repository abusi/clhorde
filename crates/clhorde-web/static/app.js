// clhorde dashboard — vanilla JS SPA
// M2: WebSocket client & state management

"use strict";

// ---------------------------------------------------------------------------
// DaemonClient — WebSocket connection with auto-reconnect
// ---------------------------------------------------------------------------

class DaemonClient {
    constructor() {
        /** @type {WebSocket|null} */
        this._ws = null;
        this._listeners = [];
        this._backoff = 1000;
        this._maxBackoff = 30000;
        this._reconnectTimer = null;
        this._intentionalClose = false;
    }

    /** Register an event callback. Returns an unsubscribe function. */
    onEvent(callback) {
        this._listeners.push(callback);
        return () => {
            this._listeners = this._listeners.filter(cb => cb !== callback);
        };
    }

    /** Connect to the daemon WebSocket. */
    connect() {
        this._intentionalClose = false;
        this._setStatus("connecting");

        const proto = location.protocol === "https:" ? "wss:" : "ws:";
        const url = `${proto}//${location.host}/api/ws`;

        try {
            this._ws = new WebSocket(url);
        } catch (e) {
            console.error("[ws] failed to create WebSocket:", e);
            this._scheduleReconnect();
            return;
        }

        this._ws.onopen = () => {
            console.log("[ws] connected");
            this._backoff = 1000;
            this._setStatus("connected");
        };

        this._ws.onmessage = (evt) => {
            try {
                const msg = JSON.parse(evt.data);
                this._dispatch(msg);
            } catch (e) {
                console.warn("[ws] bad message:", e, evt.data);
            }
        };

        this._ws.onclose = (evt) => {
            console.log("[ws] closed:", evt.code, evt.reason);
            this._ws = null;
            this._setStatus("disconnected");
            if (!this._intentionalClose) {
                this._scheduleReconnect();
            }
        };

        this._ws.onerror = (evt) => {
            console.warn("[ws] error:", evt);
            // onclose will fire after this
        };
    }

    /** Send a ClientRequest envelope. */
    send(request) {
        if (!this._ws || this._ws.readyState !== WebSocket.OPEN) {
            console.warn("[ws] not connected, cannot send");
            return false;
        }
        this._ws.send(JSON.stringify({ type: "ClientRequest", request }));
        return true;
    }

    /** Subscribe to PTY bytes for a prompt. */
    subscribePty(promptId) {
        if (!this._ws || this._ws.readyState !== WebSocket.OPEN) return false;
        this._ws.send(JSON.stringify({ type: "SubscribePty", prompt_id: promptId }));
        return true;
    }

    /** Unsubscribe from PTY bytes for a prompt. */
    unsubscribePty(promptId) {
        if (!this._ws || this._ws.readyState !== WebSocket.OPEN) return false;
        this._ws.send(JSON.stringify({ type: "UnsubscribePty", prompt_id: promptId }));
        return true;
    }

    /** Close the connection intentionally. */
    disconnect() {
        this._intentionalClose = true;
        if (this._reconnectTimer) {
            clearTimeout(this._reconnectTimer);
            this._reconnectTimer = null;
        }
        if (this._ws) {
            this._ws.close();
            this._ws = null;
        }
        this._setStatus("disconnected");
    }

    _dispatch(msg) {
        for (const cb of this._listeners) {
            try { cb(msg); } catch (e) { console.error("[ws] listener error:", e); }
        }
    }

    _setStatus(status) {
        // Update DOM
        const dot = document.getElementById("status-dot");
        const text = document.getElementById("status-text");
        if (dot) {
            dot.className = "status-dot " + status;
        }
        if (text) {
            text.textContent = status;
        }
        // Dispatch as a synthetic event
        this._dispatch({ type: "ConnectionStatus", status });
    }

    _scheduleReconnect() {
        if (this._reconnectTimer) return;
        console.log(`[ws] reconnecting in ${this._backoff}ms`);
        this._setStatus("connecting");
        this._reconnectTimer = setTimeout(() => {
            this._reconnectTimer = null;
            this.connect();
        }, this._backoff);
        this._backoff = Math.min(this._backoff * 2, this._maxBackoff);
    }
}

// ---------------------------------------------------------------------------
// AppState — reactive state derived from daemon events
// ---------------------------------------------------------------------------

class AppState {
    constructor() {
        /** @type {Array<Object>} */
        this.prompts = [];
        this.maxWorkers = 0;
        this.activeWorkers = 0;
        this.defaultMode = "interactive";
        this.connected = false;
        this._changeListeners = [];
    }

    /** Register a change callback. Returns an unsubscribe function. */
    onChange(callback) {
        this._changeListeners.push(callback);
        return () => {
            this._changeListeners = this._changeListeners.filter(cb => cb !== callback);
        };
    }

    /** Apply an incoming WebSocket message to local state. */
    update(msg) {
        switch (msg.type) {
            case "DaemonEvent":
                this._applyEvent(msg.event);
                break;
            case "ConnectionStatus":
                this.connected = msg.status === "connected";
                this._notify();
                break;
            case "Error":
                console.warn("[state] daemon error:", msg.error);
                break;
        }
    }

    /** Hydrate from a full state snapshot (GET /api/state or initial WS message). */
    hydrate(state) {
        this.prompts = state.prompts || [];
        this.maxWorkers = state.max_workers || 0;
        this.activeWorkers = state.active_workers || 0;
        this.defaultMode = state.default_mode || "interactive";
        this._sortPrompts();
        this._notify();
    }

    _applyEvent(event) {
        if (!event || !event.type) return;

        switch (event.type) {
            case "StateSnapshot":
                this.hydrate(event);
                return; // hydrate already notifies

            case "PromptAdded": {
                const existing = this.prompts.findIndex(p => p.id === event.id);
                if (existing >= 0) {
                    this.prompts[existing] = event;
                } else {
                    this.prompts.push(event);
                }
                break;
            }

            case "PromptUpdated": {
                const idx = this.prompts.findIndex(p => p.id === event.id);
                if (idx >= 0) {
                    this.prompts[idx] = event;
                } else {
                    this.prompts.push(event);
                }
                break;
            }

            case "PromptRemoved": {
                this.prompts = this.prompts.filter(p => p.id !== event.prompt_id);
                break;
            }

            case "WorkerStarted": {
                const p = this.prompts.find(p => p.id === event.prompt_id);
                if (p) p.status = "running";
                this.activeWorkers = Math.min(this.activeWorkers + 1, this.maxWorkers);
                break;
            }

            case "WorkerFinished": {
                const p = this.prompts.find(p => p.id === event.prompt_id);
                if (p) p.status = "completed";
                this.activeWorkers = Math.max(this.activeWorkers - 1, 0);
                break;
            }

            case "WorkerError": {
                const p = this.prompts.find(p => p.id === event.prompt_id);
                if (p) {
                    p.status = "failed";
                    if (event.error) p.error = event.error;
                }
                this.activeWorkers = Math.max(this.activeWorkers - 1, 0);
                break;
            }

            case "OutputChunk": {
                const p = this.prompts.find(p => p.id === event.prompt_id);
                if (p) {
                    p.output = (p.output || "") + event.text;
                    p.output_len = (p.output_len || 0) + event.text.length;
                }
                break;
            }

            case "MaxWorkersChanged":
                this.maxWorkers = event.count;
                break;

            default:
                // Unknown events are silently ignored
                return;
        }

        this._sortPrompts();
        this._notify();
    }

    /** Sort: running first, then pending, then completed/failed (newest first). */
    _sortPrompts() {
        const order = { running: 0, pending: 1, idle: 2, completed: 3, failed: 4 };
        this.prompts.sort((a, b) => {
            const oa = order[a.status] ?? 5;
            const ob = order[b.status] ?? 5;
            if (oa !== ob) return oa - ob;
            return b.id - a.id; // newest first within same status
        });
    }

    _notify() {
        for (const cb of this._changeListeners) {
            try { cb(this); } catch (e) { console.error("[state] listener error:", e); }
        }
    }
}

// ---------------------------------------------------------------------------
// UI Rendering
// ---------------------------------------------------------------------------

/** Currently selected prompt ID (from URL hash or click). */
let selectedPromptId = null;

/** Current filter state. */
let filterText = "";
let filterStatus = "all";

/** Render the prompt list in the sidebar. */
function renderPromptList(state) {
    const container = document.getElementById("prompt-list");
    const emptyState = document.getElementById("empty-state");

    const filtered = state.prompts.filter(p => {
        if (filterStatus !== "all" && p.status !== filterStatus) return false;
        if (filterText && !p.text.toLowerCase().includes(filterText.toLowerCase())) return false;
        return true;
    });

    if (filtered.length === 0) {
        emptyState.hidden = false;
        emptyState.textContent = state.prompts.length === 0
            ? "No prompts yet"
            : `No matches (${state.prompts.length} total)`;
        // Remove all prompt rows but keep empty state
        container.querySelectorAll(".prompt-row").forEach(el => el.remove());
        return;
    }

    emptyState.hidden = true;

    // Build a map of existing rows for efficient updates
    const existingRows = new Map();
    container.querySelectorAll(".prompt-row").forEach(el => {
        existingRows.set(Number(el.dataset.id), el);
    });

    // Track which IDs we need
    const neededIds = new Set(filtered.map(p => p.id));

    // Remove rows that are no longer needed
    for (const [id, el] of existingRows) {
        if (!neededIds.has(id)) {
            el.remove();
            existingRows.delete(id);
        }
    }

    // Update or create rows in order
    let prevEl = null;
    for (const prompt of filtered) {
        let row = existingRows.get(prompt.id);
        if (!row) {
            row = createPromptRow(prompt);
            existingRows.set(prompt.id, row);
        } else {
            updatePromptRow(row, prompt);
        }

        // Ensure correct order
        const expectedNext = prevEl ? prevEl.nextSibling : emptyState.nextSibling || container.firstChild;
        if (row !== expectedNext) {
            if (prevEl) {
                prevEl.after(row);
            } else {
                container.prepend(row);
            }
        }
        prevEl = row;
    }

    // Update prompt count in footer
    const countEl = document.getElementById("prompt-count");
    if (countEl) {
        const total = state.prompts.length;
        const shown = filtered.length;
        countEl.textContent = shown === total
            ? `${total} prompt${total !== 1 ? "s" : ""}`
            : `${shown} of ${total} prompts`;
    }
}

function createPromptRow(prompt) {
    const row = document.createElement("div");
    row.className = "prompt-row";
    row.dataset.id = prompt.id;
    row.addEventListener("click", () => selectPrompt(prompt.id));
    updatePromptRow(row, prompt);
    return row;
}

function updatePromptRow(row, prompt) {
    const isSelected = prompt.id === selectedPromptId;
    row.className = "prompt-row" + (isSelected ? " selected" : "");

    const statusClass = "badge badge-" + (prompt.status || "idle");
    const modeLabel = prompt.mode === "one-shot" ? "1S" : "IA";
    const wt = prompt.worktree ? '<span class="worktree-indicator">[WT]</span>' : "";
    const truncText = truncate(prompt.text || "(empty)", 50);

    row.innerHTML = `
        <span class="prompt-id">#${prompt.id}</span>
        <span class="${statusClass}">${prompt.status || "idle"}</span>
        <span class="prompt-text" title="${escapeAttr(prompt.text || "")}">${escapeHtml(truncText)}</span>
        <span class="prompt-meta">
            <span class="mode-indicator">${modeLabel}</span>
            ${wt}
        </span>
    `;
}

/** Render the footer controls. */
function renderFooter(state) {
    const maxEl = document.getElementById("max-workers");
    const activeEl = document.getElementById("active-workers");
    const modeEl = document.getElementById("mode-toggle");

    if (maxEl) maxEl.textContent = state.maxWorkers;
    if (activeEl) activeEl.textContent = state.activeWorkers;
    if (modeEl) modeEl.textContent = state.defaultMode;
}

/** Render the detail view for the selected prompt. */
function renderDetail(state) {
    const placeholder = document.getElementById("content-placeholder");
    const detail = document.getElementById("prompt-detail");
    const headerEl = document.getElementById("detail-header");
    const outputEl = document.getElementById("detail-output");

    if (selectedPromptId === null) {
        placeholder.hidden = false;
        detail.hidden = true;
        return;
    }

    const prompt = state.prompts.find(p => p.id === selectedPromptId);
    if (!prompt) {
        placeholder.hidden = false;
        detail.hidden = true;
        return;
    }

    placeholder.hidden = true;
    detail.hidden = false;

    const statusClass = "badge badge-" + (prompt.status || "idle");
    const modeLabel = prompt.mode === "one-shot" ? "one-shot" : "interactive";
    const wt = prompt.worktree ? ' <span class="worktree-indicator">[WT]</span>' : "";

    headerEl.innerHTML = `
        <span class="prompt-id">#${prompt.id}</span>
        <span class="${statusClass}">${prompt.status || "idle"}</span>
        <span class="mode-indicator">${modeLabel}</span>
        ${wt}
        <span style="flex:1"></span>
        <span class="prompt-text" style="white-space:normal;overflow:visible;text-overflow:unset">${escapeHtml(prompt.text || "")}</span>
    `;

    const output = prompt.output || prompt.full_text || "";
    if (output) {
        outputEl.textContent = output;
    } else {
        outputEl.innerHTML = `<span class="empty-state">No output yet</span>`;
    }
}

/** Full render pass. */
function render(state) {
    renderPromptList(state);
    renderFooter(state);
    renderDetail(state);
}

// ---------------------------------------------------------------------------
// Prompt selection
// ---------------------------------------------------------------------------

function selectPrompt(id) {
    selectedPromptId = id;
    location.hash = id != null ? `#prompt-${id}` : "";

    // If selecting a running interactive prompt, fetch full output
    if (id != null) {
        fetch(`/api/prompts/${id}/output`)
            .then(r => r.ok ? r.json() : null)
            .then(data => {
                if (data && data.output != null) {
                    const p = appState.prompts.find(p => p.id === id);
                    if (p) {
                        p.output = data.output;
                        renderDetail(appState);
                    }
                }
            })
            .catch(() => {});
    }

    render(appState);
}

/** Restore selection from URL hash. */
function restoreFromHash() {
    const match = location.hash.match(/^#prompt-(\d+)$/);
    if (match) {
        selectedPromptId = Number(match[1]);
    }
}

// ---------------------------------------------------------------------------
// Event wiring
// ---------------------------------------------------------------------------

const client = new DaemonClient();
const appState = new AppState();

// Wire WS messages into AppState
client.onEvent(msg => {
    appState.update(msg);
});

// Re-render on state changes
appState.onChange(state => {
    render(state);
});

// On reconnect, re-hydrate from the WS initial snapshot
// (the server sends StateSnapshot as the first message)

// ---------------------------------------------------------------------------
// DOM event handlers
// ---------------------------------------------------------------------------

function setupEventHandlers() {
    // Worker count +/-
    document.getElementById("workers-inc")?.addEventListener("click", () => {
        const newCount = appState.maxWorkers + 1;
        if (newCount > 20) return;
        fetch("/api/config/max-workers", {
            method: "PUT",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ count: newCount }),
        });
    });

    document.getElementById("workers-dec")?.addEventListener("click", () => {
        const newCount = appState.maxWorkers - 1;
        if (newCount < 1) return;
        fetch("/api/config/max-workers", {
            method: "PUT",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ count: newCount }),
        });
    });

    // Mode toggle
    document.getElementById("mode-toggle")?.addEventListener("click", () => {
        const newMode = appState.defaultMode === "interactive" ? "one-shot" : "interactive";
        fetch("/api/config/default-mode", {
            method: "PUT",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ mode: newMode }),
        });
    });

    // Search input
    document.getElementById("search-input")?.addEventListener("input", (e) => {
        filterText = e.target.value;
        render(appState);
    });

    // Escape to clear search
    document.getElementById("search-input")?.addEventListener("keydown", (e) => {
        if (e.key === "Escape") {
            e.target.value = "";
            filterText = "";
            render(appState);
        }
    });

    // Status filter chips
    document.getElementById("status-filters")?.addEventListener("click", (e) => {
        const chip = e.target.closest(".chip");
        if (!chip) return;
        filterStatus = chip.dataset.filter;
        // Update active chip
        document.querySelectorAll("#status-filters .chip").forEach(c => {
            c.classList.toggle("active", c.dataset.filter === filterStatus);
        });
        render(appState);
    });

    // Hash change
    window.addEventListener("hashchange", () => {
        restoreFromHash();
        render(appState);
    });
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

function truncate(str, max) {
    return str.length > max ? str.slice(0, max - 1) + "\u2026" : str;
}

function escapeHtml(str) {
    return str.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function escapeAttr(str) {
    return str.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

restoreFromHash();
setupEventHandlers();
client.connect();

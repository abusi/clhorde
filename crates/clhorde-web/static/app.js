// clhorde dashboard — vanilla JS SPA

"use strict";

// ---------------------------------------------------------------------------
// Auth token management
// ---------------------------------------------------------------------------

const AUTH_TOKEN_KEY = "clhorde_auth_token";

function getAuthToken() {
    return localStorage.getItem(AUTH_TOKEN_KEY);
}

function setAuthToken(token) {
    localStorage.setItem(AUTH_TOKEN_KEY, token);
}

function clearAuthToken() {
    localStorage.removeItem(AUTH_TOKEN_KEY);
}

/** Build headers object with auth token if present. */
function authHeaders(extra = {}) {
    const headers = { ...extra };
    const token = getAuthToken();
    if (token) {
        headers["Authorization"] = `Bearer ${token}`;
    }
    return headers;
}

/** Show the login overlay, hide on successful auth. */
function showLogin(errorMsg) {
    const overlay = document.getElementById("login-overlay");
    const errorEl = document.getElementById("login-error");
    if (overlay) overlay.hidden = false;
    if (errorEl) errorEl.textContent = errorMsg || "";
}

function hideLogin() {
    const overlay = document.getElementById("login-overlay");
    if (overlay) overlay.hidden = true;
}

function updateLogoutButton() {
    const btn = document.getElementById("logout-btn");
    if (btn) btn.hidden = !getAuthToken();
}

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
        let url = `${proto}//${location.host}/api/ws`;
        const token = getAuthToken();
        if (token) {
            url += `?token=${encodeURIComponent(token)}`;
        }

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
        const st = (p.status || "").toLowerCase();
        if (filterStatus !== "all" && st !== filterStatus) return false;
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

    const st = (prompt.status || "idle").toLowerCase();
    const statusClass = "badge badge-" + st;
    const modeLabel = prompt.mode === "one-shot" ? "1S" : "IA";
    const wt = prompt.worktree ? '<span class="worktree-indicator">[WT]</span>' : "";
    const truncText = truncate(prompt.text || "(empty)", 50);

    row.innerHTML = `
        <span class="prompt-id">#${prompt.id}</span>
        <span class="${statusClass}">${st}</span>
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

/** Whether auto-scroll is enabled for the output viewer. */
let autoScroll = true;

/** Render the detail view for the selected prompt. */
function renderDetail(state) {
    const placeholder = document.getElementById("content-placeholder");
    const detail = document.getElementById("prompt-detail");
    const headerEl = document.getElementById("detail-header");
    const actionsEl = document.getElementById("detail-actions");
    const outputEl = document.getElementById("detail-output");
    const terminalEl = document.getElementById("detail-terminal");
    const inputEl = document.getElementById("detail-input");

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

    const isInteractive = prompt.mode === "interactive";
    const st = (prompt.status || "").toLowerCase();
    const isRunning = st === "running" || st === "idle";
    const isOneShot = prompt.mode !== "interactive";
    const isDone = st === "completed" || st === "failed";
    const isPending = st === "pending";

    // --- Header ---
    const statusClass = "badge badge-" + st;
    const modeLabel = isInteractive ? "interactive" : "one-shot";
    const wt = prompt.worktree ? ' <span class="worktree-indicator">[WT]</span>' : "";
    const elapsed = prompt.elapsed_secs ? ` <span class="mode-indicator">${formatDuration(prompt.elapsed_secs)}</span>` : "";
    const sessionBadge = prompt.session_id
        ? ` <span class="mode-indicator" title="${escapeAttr(prompt.session_id)}">session: ${escapeHtml(prompt.session_id.slice(0, 8))}…</span>`
        : "";

    headerEl.innerHTML = `
        <span class="prompt-id">#${prompt.id}</span>
        <span class="${statusClass}">${st}</span>
        <span class="mode-indicator">${modeLabel}</span>
        ${wt}${elapsed}${sessionBadge}
        <span style="flex:1"></span>
        <span class="prompt-text" style="white-space:normal;overflow:visible;text-overflow:unset">${escapeHtml(prompt.text || "")}</span>
    `;

    // --- Action buttons ---
    renderActions(actionsEl, prompt, isRunning, isDone, isPending);

    // --- Output / Terminal ---
    if (isInteractive) {
        outputEl.hidden = true;
        terminalEl.hidden = false;
        attachTerminal(terminalEl, prompt.id);
    } else {
        // One-shot prompts use ANSI output viewer
        outputEl.hidden = false;
        terminalEl.hidden = true;
        renderOutput(outputEl, prompt);
    }

    // --- Follow-up input ---
    // Show for running one-shot prompts only
    const showInput = isRunning && isOneShot;
    inputEl.hidden = !showInput;
}

/** Render action buttons based on prompt status. */
function renderActions(container, prompt, isRunning, isDone, isPending) {
    const buttons = [];

    if (isRunning) {
        buttons.push(`<button class="btn btn-sm btn-danger" data-action="kill" data-id="${prompt.id}">Kill</button>`);
    }
    if (isDone) {
        buttons.push(`<button class="btn btn-sm btn-success" data-action="retry" data-id="${prompt.id}">Retry</button>`);
        buttons.push(`<button class="btn btn-sm" data-action="resume" data-id="${prompt.id}">Resume</button>`);
    }
    if (isPending) {
        buttons.push(`<button class="btn btn-sm" data-action="move-up" data-id="${prompt.id}">Move Up</button>`);
        buttons.push(`<button class="btn btn-sm" data-action="move-down" data-id="${prompt.id}">Move Down</button>`);
    }
    buttons.push(`<button class="btn btn-sm btn-danger" data-action="delete" data-id="${prompt.id}">Delete</button>`);

    container.innerHTML = buttons.join("");
}

/** Render one-shot output with ANSI color support. */
function renderOutput(el, prompt) {
    const output = prompt.output || prompt.full_text || "";

    if (!output) {
        const msg = (prompt.status || "").toLowerCase() === "running" ? "Waiting for output..." : "No output";
        el.innerHTML = `<span class="empty-state">${msg}</span>`;
        return;
    }

    el.innerHTML = ansiToHtml(output);
    el.className = "detail-output ansi-output";

    // Auto-scroll to bottom
    if (autoScroll) {
        el.scrollTop = el.scrollHeight;
    }
}

// ---------------------------------------------------------------------------
// ANSI escape code parser
// ---------------------------------------------------------------------------

/** Convert text with ANSI escape codes to styled HTML. */
function ansiToHtml(text) {
    const out = [];
    let i = 0;
    let currentStyles = [];

    while (i < text.length) {
        // Check for ESC[...m sequences
        if (text[i] === "\x1b" && text[i + 1] === "[") {
            const end = text.indexOf("m", i + 2);
            if (end !== -1 && end - i < 20) {
                const codes = text.slice(i + 2, end).split(";").map(Number);
                currentStyles = applyAnsiCodes(currentStyles, codes);
                i = end + 1;
                continue;
            }
        }

        // Skip other ESC sequences (e.g., cursor movement)
        if (text[i] === "\x1b" && text[i + 1] === "[") {
            const match = text.slice(i).match(/^\x1b\[[0-9;]*[A-Za-z]/);
            if (match) {
                i += match[0].length;
                continue;
            }
        }

        // Collect normal text until next ESC or end
        let textStart = i;
        while (i < text.length && text[i] !== "\x1b") i++;
        const chunk = text.slice(textStart, i);

        if (currentStyles.length > 0) {
            const classes = currentStyles.filter(s => s.startsWith("ansi-")).join(" ");
            const inlineStyle = currentStyles.filter(s => s.startsWith("color:") || s.startsWith("background")).join(";");
            const classAttr = classes ? ` class="${classes}"` : "";
            const styleAttr = inlineStyle ? ` style="${inlineStyle}"` : "";
            out.push(`<span${classAttr}${styleAttr}>${escapeHtml(chunk)}</span>`);
        } else {
            out.push(escapeHtml(chunk));
        }
    }

    return out.join("");
}

/** Map ANSI SGR codes to CSS classes/styles. */
function applyAnsiCodes(current, codes) {
    const styles = [];

    for (let j = 0; j < codes.length; j++) {
        const c = codes[j];

        if (c === 0 || isNaN(c)) {
            // Reset
            return [];
        } else if (c === 1) {
            styles.push("ansi-bold");
        } else if (c === 2) {
            styles.push("ansi-dim");
        } else if (c === 3) {
            styles.push("ansi-italic");
        } else if (c === 4) {
            styles.push("ansi-underline");
        } else if (c === 9) {
            styles.push("ansi-strikethrough");
        } else if (c >= 30 && c <= 37) {
            styles.push(`color:${ansi4Color(c - 30)}`);
        } else if (c === 38 && codes[j + 1] === 5) {
            // 256-color: ESC[38;5;Nm
            styles.push(`color:${ansi256Color(codes[j + 2])}`);
            j += 2;
        } else if (c === 38 && codes[j + 1] === 2) {
            // True color: ESC[38;2;R;G;Bm
            styles.push(`color:rgb(${codes[j+2]},${codes[j+3]},${codes[j+4]})`);
            j += 4;
        } else if (c >= 40 && c <= 47) {
            styles.push(`background-color:${ansi4Color(c - 40)}`);
        } else if (c === 48 && codes[j + 1] === 5) {
            styles.push(`background-color:${ansi256Color(codes[j + 2])}`);
            j += 2;
        } else if (c === 48 && codes[j + 1] === 2) {
            styles.push(`background-color:rgb(${codes[j+2]},${codes[j+3]},${codes[j+4]})`);
            j += 4;
        } else if (c >= 90 && c <= 97) {
            styles.push(`color:${ansi4BrightColor(c - 90)}`);
        } else if (c >= 100 && c <= 107) {
            styles.push(`background-color:${ansi4BrightColor(c - 100)}`);
        }
    }

    // Merge with existing styles (replace colors, accumulate decorations)
    const merged = [...current];
    for (const s of styles) {
        if (s.startsWith("color:")) {
            const idx = merged.findIndex(m => m.startsWith("color:"));
            if (idx >= 0) merged[idx] = s; else merged.push(s);
        } else if (s.startsWith("background")) {
            const idx = merged.findIndex(m => m.startsWith("background"));
            if (idx >= 0) merged[idx] = s; else merged.push(s);
        } else if (!merged.includes(s)) {
            merged.push(s);
        }
    }
    return merged;
}

const ANSI_4 = ["#000","#c33","#3c3","#cc3","#33c","#c3c","#3cc","#ccc"];
const ANSI_4_BRIGHT = ["#555","#f55","#5f5","#ff5","#55f","#f5f","#5ff","#fff"];

function ansi4Color(n) { return ANSI_4[n] || "#ccc"; }
function ansi4BrightColor(n) { return ANSI_4_BRIGHT[n] || "#fff"; }

function ansi256Color(n) {
    if (n < 8) return ANSI_4[n];
    if (n < 16) return ANSI_4_BRIGHT[n - 8];
    if (n < 232) {
        // 216-color cube
        const idx = n - 16;
        const r = Math.floor(idx / 36) * 51;
        const g = Math.floor((idx % 36) / 6) * 51;
        const b = (idx % 6) * 51;
        return `rgb(${r},${g},${b})`;
    }
    // Grayscale
    const v = (n - 232) * 10 + 8;
    return `rgb(${v},${v},${v})`;
}

function formatDuration(secs) {
    if (secs < 60) return `${Math.round(secs)}s`;
    const m = Math.floor(secs / 60);
    const s = Math.round(secs % 60);
    return `${m}m${s}s`;
}

/** Full render pass. */
function render(state) {
    renderPromptList(state);
    renderFooter(state);
    renderDetail(state);
}

// ---------------------------------------------------------------------------
// xterm.js terminal management
// ---------------------------------------------------------------------------

/** Currently active terminal instance. */
let activeTerm = null;
let activeTermPromptId = null;
let ptyUnsubscribe = null;

/** Attach or reuse an xterm.js terminal for the given prompt. */
function attachTerminal(container, promptId) {
    // Already attached to this prompt
    if (activeTermPromptId === promptId && activeTerm) return;

    // Clean up previous terminal
    detachTerminal();

    // Check if xterm.js is loaded
    if (typeof Terminal === "undefined") {
        container.innerHTML = `<div class="empty-state">xterm.js not loaded</div>`;
        return;
    }

    activeTermPromptId = promptId;

    const term = new Terminal({
        theme: {
            background: "#0f1117",
            foreground: "#e4e4e8",
            cursor: "#e4e4e8",
            cursorAccent: "#0f1117",
            selectionBackground: "#2a2d40",
            black: "#000000",
            red: "#c33",
            green: "#3c3",
            yellow: "#cc3",
            blue: "#33c",
            magenta: "#c3c",
            cyan: "#3cc",
            white: "#ccc",
            brightBlack: "#555",
            brightRed: "#f55",
            brightGreen: "#5f5",
            brightYellow: "#ff5",
            brightBlue: "#55f",
            brightMagenta: "#f5f",
            brightCyan: "#5ff",
            brightWhite: "#fff",
        },
        fontSize: 14,
        fontFamily: "'JetBrains Mono', 'Cascadia Code', 'Fira Code', 'Source Code Pro', monospace",
        cursorBlink: true,
        scrollback: 5000,
        convertEol: false,
    });

    // Load addons
    if (typeof FitAddon !== "undefined") {
        const fitAddon = new FitAddon.FitAddon();
        term.loadAddon(fitAddon);
        term._fitAddon = fitAddon;
    }
    if (typeof WebLinksAddon !== "undefined") {
        term.loadAddon(new WebLinksAddon.WebLinksAddon());
    }

    container.innerHTML = "";
    term.open(container);

    // Load WebGL renderer for GPU-accelerated rendering (falls back to DOM on failure)
    if (typeof WebglAddon !== "undefined") {
        try {
            const webgl = new WebglAddon.WebglAddon();
            webgl.onContextLoss(() => {
                console.warn("[term] WebGL context lost, falling back to DOM renderer");
                webgl.dispose();
            });
            term.loadAddon(webgl);
        } catch (e) {
            console.warn("[term] WebGL addon failed to load, using DOM renderer:", e);
        }
    }

    // Fit to container
    if (term._fitAddon) {
        try { term._fitAddon.fit(); } catch (e) {}
    }

    activeTerm = term;

    // Subscribe to PTY bytes for this prompt
    client.subscribePty(promptId);

    // Listen for PTY bytes
    ptyUnsubscribe = client.onEvent((msg) => {
        if (msg.type === "PtyBytes" && msg.prompt_id === promptId && activeTerm) {
            try {
                const binary = atob(msg.data);
                const bytes = new Uint8Array(binary.length);
                for (let i = 0; i < binary.length; i++) {
                    bytes[i] = binary.charCodeAt(i);
                }
                activeTerm.write(bytes);
            } catch (e) {
                console.warn("[term] failed to decode PTY bytes:", e);
            }
        }
    });

    // Forward keyboard input to daemon
    term.onData((data) => {
        // Send raw bytes via WS ClientRequest SendBytes
        const encoded = btoa(data);
        client.send({ type: "SendBytes", prompt_id: promptId, data: Array.from(data, c => c.charCodeAt(0)) });
    });

    // Handle resize — fit terminal and notify daemon of new dimensions
    const resizeObserver = new ResizeObserver(() => {
        if (activeTerm && activeTerm._fitAddon) {
            try {
                activeTerm._fitAddon.fit();
                // Send new dimensions to the daemon so the PTY resizes to match
                client.send({
                    type: "ResizePty",
                    prompt_id: promptId,
                    cols: activeTerm.cols,
                    rows: activeTerm.rows,
                });
            } catch (e) {}
        }
    });
    resizeObserver.observe(container);
    term._resizeObserver = resizeObserver;
}

/** Detach and clean up the current terminal. */
function detachTerminal() {
    if (activeTermPromptId !== null) {
        client.unsubscribePty(activeTermPromptId);
    }
    if (ptyUnsubscribe) {
        ptyUnsubscribe();
        ptyUnsubscribe = null;
    }
    if (activeTerm) {
        if (activeTerm._resizeObserver) {
            activeTerm._resizeObserver.disconnect();
        }
        activeTerm.dispose();
        activeTerm = null;
    }
    activeTermPromptId = null;
}

// ---------------------------------------------------------------------------
// Prompt selection
// ---------------------------------------------------------------------------

function selectPrompt(id) {
    // Clean up terminal if switching away from an interactive prompt
    if (activeTermPromptId !== null && activeTermPromptId !== id) {
        detachTerminal();
    }

    selectedPromptId = id;
    location.hash = id != null ? `#prompt-${id}` : "";

    // Fetch full output for the selected prompt
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

    // Action buttons (event delegation)
    document.getElementById("detail-actions")?.addEventListener("click", async (e) => {
        const btn = e.target.closest("[data-action]");
        if (!btn) return;

        const action = btn.dataset.action;
        const id = Number(btn.dataset.id);

        // Confirm destructive actions
        if (action === "kill" && !confirm(`Kill worker for prompt #${id}?`)) return;
        if (action === "delete" && !confirm(`Delete prompt #${id}?`)) return;

        btn.disabled = true;

        const routes = {
            "kill":      { method: "POST",   url: `/api/prompts/${id}/kill` },
            "retry":     { method: "POST",   url: `/api/prompts/${id}/retry` },
            "resume":    { method: "POST",   url: `/api/prompts/${id}/resume` },
            "delete":    { method: "DELETE", url: `/api/prompts/${id}` },
            "move-up":   { method: "POST",   url: `/api/prompts/${id}/move-up` },
            "move-down": { method: "POST",   url: `/api/prompts/${id}/move-down` },
        };

        const route = routes[action];
        if (!route) return;

        try {
            await fetch(route.url, { method: route.method });
            // If deleted, deselect
            if (action === "delete") selectPrompt(null);
        } catch (err) {
            console.error(`Action ${action} failed:`, err);
        } finally {
            btn.disabled = false;
        }
    });

    // Follow-up input
    const followupInput = document.getElementById("followup-input");
    const followupSend = document.getElementById("followup-send");

    async function sendFollowup() {
        const text = followupInput?.value?.trim();
        if (!text || selectedPromptId === null) return;

        followupSend.disabled = true;
        try {
            const res = await fetch(`/api/prompts/${selectedPromptId}/input`, {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ text }),
            });
            if (res.ok) {
                followupInput.value = "";
                followupInput.classList.add("followup-flash");
                setTimeout(() => followupInput.classList.remove("followup-flash"), 400);
            }
        } catch (err) {
            console.error("Follow-up send failed:", err);
        } finally {
            followupSend.disabled = false;
        }
    }

    followupSend?.addEventListener("click", sendFollowup);
    followupInput?.addEventListener("keydown", (e) => {
        if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            sendFollowup();
        }
    });

    // CWD toggle
    document.getElementById("cwd-toggle")?.addEventListener("click", () => {
        const row = document.getElementById("cwd-row");
        if (row) {
            row.hidden = !row.hidden;
            if (!row.hidden) document.getElementById("submit-cwd")?.focus();
        }
    });

    // Prompt submission form
    const submitForm = document.getElementById("submit-form");
    const promptInput = document.getElementById("prompt-input");
    const submitBtn = document.getElementById("submit-btn");
    const submitError = document.getElementById("submit-error");

    if (submitForm) {
        submitForm.addEventListener("submit", (e) => {
            e.preventDefault();
            submitPrompt();
        });
    }

    // Ctrl+Enter to submit from textarea
    if (promptInput) {
        promptInput.addEventListener("keydown", (e) => {
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
                e.preventDefault();
                submitPrompt();
            }
        });
    }

    async function submitPrompt() {
        const text = promptInput?.value?.trim();
        if (!text) return;

        const mode = document.getElementById("submit-mode")?.value || "interactive";
        const worktree = document.getElementById("submit-worktree")?.checked || false;
        const cwdInput = document.getElementById("submit-cwd")?.value?.trim();
        const cwd = cwdInput || null;

        // Disable button during submission
        if (submitBtn) submitBtn.disabled = true;
        if (submitError) submitError.textContent = "";

        try {
            const res = await fetch("/api/prompts", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ text, mode, worktree, cwd }),
            });

            if (res.ok) {
                // Apply the PromptAdded response immediately instead of
                // waiting for the WebSocket broadcast (which may race).
                const info = await res.json().catch(() => null);
                if (info) {
                    appState._applyEvent({ type: "PromptAdded", ...info });
                    appState._notify();
                }
                if (promptInput) promptInput.value = "";
                if (submitError) submitError.textContent = "";
            } else {
                const data = await res.json().catch(() => null);
                const msg = data?.error || `Error ${res.status}`;
                if (submitError) submitError.textContent = msg;
            }
        } catch (e) {
            if (submitError) submitError.textContent = "Failed to connect";
        } finally {
            if (submitBtn) submitBtn.disabled = false;
        }
    }
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
// Toast notifications (M2)
// ---------------------------------------------------------------------------

/** Show a toast notification. Returns the toast element. */
function showToast(message, type = "error", options = {}) {
    const container = document.getElementById("toast-container");
    if (!container) return null;

    const toast = document.createElement("div");
    toast.className = `toast toast-${type}`;

    let html = `<span class="toast-msg">${escapeHtml(message)}</span>`;
    if (options.retry) {
        html += `<button class="toast-retry">Retry</button>`;
    }
    html += `<button class="toast-dismiss">\u00d7</button>`;
    toast.innerHTML = html;

    toast.querySelector(".toast-dismiss")?.addEventListener("click", () => toast.remove());
    if (options.retry) {
        toast.querySelector(".toast-retry")?.addEventListener("click", () => {
            toast.remove();
            options.retry();
        });
    }

    container.appendChild(toast);

    // Auto-dismiss after 5s unless persistent
    if (!options.persistent) {
        setTimeout(() => toast.remove(), 5000);
    }

    return toast;
}

// ---------------------------------------------------------------------------
// API fetch wrapper with timeout and error toasts (M2)
// ---------------------------------------------------------------------------

/** Fetch with 10s timeout, auth headers, and error toast on failure. */
async function apiFetch(url, options = {}) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 10000);

    // Merge auth headers
    options.headers = authHeaders(options.headers || {});

    try {
        const res = await fetch(url, { ...options, signal: controller.signal });
        clearTimeout(timeout);

        if (res.status === 401) {
            showLogin("Invalid or expired token");
            return res;
        }

        if (!res.ok) {
            const data = await res.json().catch(() => null);
            const msg = data?.error || `HTTP ${res.status}`;
            showToast(msg, "error", {
                retry: () => apiFetch(url, options),
            });
        }
        return res;
    } catch (e) {
        clearTimeout(timeout);
        const msg = e.name === "AbortError" ? "Request timed out" : "Network error";
        showToast(msg, "error", {
            retry: () => apiFetch(url, options),
        });
        return null;
    }
}

// ---------------------------------------------------------------------------
// Reconnection banner (M2)
// ---------------------------------------------------------------------------

let bannerVisible = false;
let pollInterval = null;

client.onEvent(msg => {
    if (msg.type !== "ConnectionStatus") return;
    const banner = document.getElementById("reconnect-banner");
    const bannerText = document.getElementById("banner-text");

    if (msg.status === "connecting" || msg.status === "disconnected") {
        if (banner) banner.hidden = false;
        if (bannerText) {
            bannerText.textContent = msg.status === "connecting"
                ? "Reconnecting to daemon..."
                : "Disconnected from daemon";
        }
        bannerVisible = true;

        // Start fallback polling if WS is down
        if (!pollInterval) {
            pollInterval = setInterval(async () => {
                try {
                    const res = await fetch("/api/state", { headers: authHeaders() });
                    if (res.ok) {
                        const data = await res.json();
                        appState.hydrate(data);
                    }
                } catch (e) { /* ignore */ }
            }, 5000);
        }
    } else if (msg.status === "connected") {
        if (banner) banner.hidden = true;
        bannerVisible = false;

        // Stop fallback polling
        if (pollInterval) {
            clearInterval(pollInterval);
            pollInterval = null;
        }
    }
});

// ---------------------------------------------------------------------------
// View navigation (M1)
// ---------------------------------------------------------------------------

let currentView = "dashboard";

function switchView(view) {
    currentView = view;

    // Toggle nav tabs
    document.querySelectorAll(".nav-tab").forEach(tab => {
        tab.classList.toggle("active", tab.dataset.view === view);
    });

    // Toggle views
    const sidebar = document.getElementById("sidebar");
    const content = document.getElementById("content");
    const storeView = document.getElementById("store-view");

    if (view === "dashboard") {
        if (sidebar) sidebar.hidden = false;
        if (content) content.hidden = false;
        if (storeView) storeView.hidden = true;
    } else if (view === "store") {
        if (sidebar) sidebar.hidden = true;
        if (content) content.hidden = true;
        if (storeView) storeView.hidden = false;
        loadStoreData();
    }
}

// ---------------------------------------------------------------------------
// Store management (M1)
// ---------------------------------------------------------------------------

async function loadStoreData() {
    const hdrs = { headers: authHeaders() };
    const [listRes, countRes, pathRes] = await Promise.all([
        fetch("/api/store", hdrs).catch(() => null),
        fetch("/api/store/count", hdrs).catch(() => null),
        fetch("/api/store/path", hdrs).catch(() => null),
    ]);

    // Counts
    const countsEl = document.getElementById("store-counts");
    if (countRes?.ok && countsEl) {
        const counts = await countRes.json();
        countsEl.innerHTML = [
            { label: "Pending", count: counts.pending, color: "pending" },
            { label: "Running", count: counts.running, color: "running" },
            { label: "Completed", count: counts.completed, color: "completed" },
            { label: "Failed", count: counts.failed, color: "failed" },
        ].map(c => `
            <div class="store-count-card">
                <div class="count" style="color:var(--status-${c.color})">${c.count}</div>
                <div class="label">${c.label}</div>
            </div>
        `).join("");
    }

    // Path
    const pathEl = document.getElementById("store-path");
    if (pathRes?.ok && pathEl) {
        const data = await pathRes.json();
        pathEl.textContent = data.path || "";
    }

    // List
    const listEl = document.getElementById("store-list");
    if (listRes?.ok && listEl) {
        const prompts = await listRes.json();
        if (prompts.length === 0) {
            listEl.innerHTML = `<div class="empty-state">No stored prompts</div>`;
        } else {
            listEl.innerHTML = prompts.map(p => {
                const statusClass = "badge badge-" + (p.status || "idle");
                const modeLabel = p.mode === "one-shot" ? "1S" : "IA";
                return `
                    <div class="store-row">
                        <span class="prompt-id">#${p.id}</span>
                        <span class="${statusClass}">${p.status || "idle"}</span>
                        <span class="mode-indicator">${modeLabel}</span>
                        <span class="prompt-text">${escapeHtml(truncate(p.text || "", 80))}</span>
                    </div>
                `;
            }).join("");
        }
    }
}

async function storeAction(action) {
    const routes = {
        "drop-all":        { url: "/api/store/drop", body: { filter: "all" } },
        "drop-completed":  { url: "/api/store/drop", body: { filter: "completed" } },
        "drop-failed":     { url: "/api/store/drop", body: { filter: "failed" } },
        "keep-completed":  { url: "/api/store/keep", body: { filter: "completed" } },
        "clean-worktrees": { url: "/api/store/clean-worktrees", body: null },
        "refresh":         { url: null },
    };

    const route = routes[action];
    if (!route) return;

    if (action.startsWith("drop") && !confirm(`${action.replace("-", " ")}?`)) return;

    if (route.url) {
        try {
            const res = await fetch(route.url, {
                method: "POST",
                headers: authHeaders(route.body ? { "Content-Type": "application/json" } : {}),
                body: route.body ? JSON.stringify(route.body) : undefined,
            });
            if (res.ok) {
                const data = await res.json();
                if (data.message) showToast(data.message, "success");
            } else {
                const data = await res.json().catch(() => null);
                showToast(data?.error || "Store action failed", "error");
            }
        } catch (e) {
            showToast("Failed to connect", "error");
        }
    }

    // Refresh store data after any action
    loadStoreData();
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

restoreFromHash();
setupEventHandlers();

// Nav tabs
document.querySelectorAll(".nav-tab").forEach(tab => {
    tab.addEventListener("click", () => switchView(tab.dataset.view));
});

// Store action buttons
document.querySelector(".store-actions")?.addEventListener("click", (e) => {
    const btn = e.target.closest("[data-store]");
    if (btn) storeAction(btn.dataset.store);
});

// Login form handler
document.getElementById("login-form")?.addEventListener("submit", async (e) => {
    e.preventDefault();
    const tokenInput = document.getElementById("login-token");
    const errorEl = document.getElementById("login-error");
    const token = tokenInput?.value.trim();
    if (!token) {
        if (errorEl) errorEl.textContent = "Token is required";
        return;
    }
    // Test the token against the health endpoint
    try {
        const res = await fetch("/api/health", {
            headers: { "Authorization": `Bearer ${token}` },
        });
        if (res.status === 401) {
            if (errorEl) errorEl.textContent = "Invalid token";
            return;
        }
        // Token is valid — save and reconnect
        setAuthToken(token);
        hideLogin();
        updateLogoutButton();
        // Reconnect WS with new token
        client.disconnect();
        client.connect();
    } catch (err) {
        if (errorEl) errorEl.textContent = "Cannot reach server";
    }
});

// Logout button
document.getElementById("logout-btn")?.addEventListener("click", () => {
    clearAuthToken();
    updateLogoutButton();
    client.disconnect();
    client.connect();
});

// On initial load, check if auth is required by probing the health endpoint
(async function checkAuth() {
    try {
        const res = await fetch("/api/health", { headers: authHeaders() });
        if (res.status === 401) {
            showLogin();
            return;
        }
    } catch (e) {
        // Server unreachable — proceed anyway, WS will handle reconnect
    }
    hideLogin();
    updateLogoutButton();
})();

client.connect();

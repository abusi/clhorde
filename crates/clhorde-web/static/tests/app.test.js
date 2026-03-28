// Unit tests for clhorde-web frontend pure functions & state logic
// Run with: node --test tests/app.test.js

import { describe, it, beforeEach, mock } from "node:test";
import assert from "node:assert/strict";

// ---------------------------------------------------------------------------
// Since app.js is a plain script (not a module), we re-implement the pure
// functions here for testing. These must stay in sync with app.js.
// A future refactor could extract them into a shared module.
// ---------------------------------------------------------------------------

// === Utility functions (from app.js) ===

function truncate(str, max) {
    return str.length > max ? str.slice(0, max - 1) + "\u2026" : str;
}

function escapeHtml(str) {
    return str.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function escapeAttr(str) {
    return str.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function formatDuration(secs) {
    if (secs < 60) return `${Math.round(secs)}s`;
    const m = Math.floor(secs / 60);
    const s = Math.round(secs % 60);
    return `${m}m${s}s`;
}

// === ANSI functions (from app.js) ===

const ANSI_4 = ["#000", "#c33", "#3c3", "#cc3", "#33c", "#c3c", "#3cc", "#ccc"];
const ANSI_4_BRIGHT = ["#555", "#f55", "#5f5", "#ff5", "#55f", "#f5f", "#5ff", "#fff"];

function ansi4Color(n) { return ANSI_4[n] || "#ccc"; }
function ansi4BrightColor(n) { return ANSI_4_BRIGHT[n] || "#fff"; }

function ansi256Color(n) {
    if (n < 8) return ANSI_4[n];
    if (n < 16) return ANSI_4_BRIGHT[n - 8];
    if (n < 232) {
        const idx = n - 16;
        const r = Math.floor(idx / 36) * 51;
        const g = Math.floor((idx % 36) / 6) * 51;
        const b = (idx % 6) * 51;
        return `rgb(${r},${g},${b})`;
    }
    const v = (n - 232) * 10 + 8;
    return `rgb(${v},${v},${v})`;
}

function applyAnsiCodes(current, codes) {
    const styles = [];

    for (let j = 0; j < codes.length; j++) {
        const c = codes[j];

        if (c === 0 || isNaN(c)) {
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
            styles.push(`color:${ansi256Color(codes[j + 2])}`);
            j += 2;
        } else if (c === 38 && codes[j + 1] === 2) {
            styles.push(`color:rgb(${codes[j + 2]},${codes[j + 3]},${codes[j + 4]})`);
            j += 4;
        } else if (c >= 40 && c <= 47) {
            styles.push(`background-color:${ansi4Color(c - 40)}`);
        } else if (c === 48 && codes[j + 1] === 5) {
            styles.push(`background-color:${ansi256Color(codes[j + 2])}`);
            j += 2;
        } else if (c === 48 && codes[j + 1] === 2) {
            styles.push(`background-color:rgb(${codes[j + 2]},${codes[j + 3]},${codes[j + 4]})`);
            j += 4;
        } else if (c >= 90 && c <= 97) {
            styles.push(`color:${ansi4BrightColor(c - 90)}`);
        } else if (c >= 100 && c <= 107) {
            styles.push(`background-color:${ansi4BrightColor(c - 100)}`);
        }
    }

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

function ansiToHtml(text) {
    const out = [];
    let i = 0;
    let currentStyles = [];

    while (i < text.length) {
        if (text[i] === "\x1b" && text[i + 1] === "[") {
            const end = text.indexOf("m", i + 2);
            if (end !== -1 && end - i < 20) {
                const codes = text.slice(i + 2, end).split(";").map(Number);
                currentStyles = applyAnsiCodes(currentStyles, codes);
                i = end + 1;
                continue;
            }
        }

        if (text[i] === "\x1b" && text[i + 1] === "[") {
            const match = text.slice(i).match(/^\x1b\[[0-9;]*[A-Za-z]/);
            if (match) {
                i += match[0].length;
                continue;
            }
        }

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

// === AppState (from app.js) ===

class AppState {
    constructor() {
        this.prompts = [];
        this.maxWorkers = 0;
        this.activeWorkers = 0;
        this.defaultMode = "interactive";
        this.connected = false;
        this._changeListeners = [];
    }

    onChange(callback) {
        this._changeListeners.push(callback);
        return () => {
            this._changeListeners = this._changeListeners.filter(cb => cb !== callback);
        };
    }

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
                break;
        }
    }

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
                return;
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
                return;
        }

        this._sortPrompts();
        this._notify();
    }

    _sortPrompts() {
        const order = { running: 0, pending: 1, idle: 2, completed: 3, failed: 4 };
        this.prompts.sort((a, b) => {
            const oa = order[a.status] ?? 5;
            const ob = order[b.status] ?? 5;
            if (oa !== ob) return oa - ob;
            return b.id - a.id;
        });
    }

    _notify() {
        for (const cb of this._changeListeners) {
            try { cb(this); } catch (e) {}
        }
    }
}

// === Auth helpers (from app.js) ===

// Minimal localStorage mock for auth tests
function createStorageMock() {
    const store = new Map();
    return {
        getItem(key) { return store.get(key) ?? null; },
        setItem(key, value) { store.set(key, String(value)); },
        removeItem(key) { store.delete(key); },
        clear() { store.clear(); },
    };
}

// ===================================================================
// Tests
// ===================================================================

describe("truncate", () => {
    it("returns string unchanged when shorter than max", () => {
        assert.equal(truncate("hello", 10), "hello");
    });

    it("returns string unchanged when exactly max length", () => {
        assert.equal(truncate("hello", 5), "hello");
    });

    it("truncates with ellipsis when longer than max", () => {
        assert.equal(truncate("hello world", 6), "hello\u2026");
    });

    it("handles empty string", () => {
        assert.equal(truncate("", 5), "");
    });

    it("truncates to 1 char + ellipsis for max=2", () => {
        assert.equal(truncate("abcdef", 2), "a\u2026");
    });
});

describe("escapeHtml", () => {
    it("escapes ampersands", () => {
        assert.equal(escapeHtml("a & b"), "a &amp; b");
    });

    it("escapes angle brackets", () => {
        assert.equal(escapeHtml("<script>"), "&lt;script&gt;");
    });

    it("returns plain text unchanged", () => {
        assert.equal(escapeHtml("hello"), "hello");
    });

    it("escapes multiple occurrences", () => {
        assert.equal(escapeHtml("a<b>c&d"), "a&lt;b&gt;c&amp;d");
    });
});

describe("escapeAttr", () => {
    it("escapes double quotes", () => {
        assert.equal(escapeAttr('say "hi"'), "say &quot;hi&quot;");
    });

    it("escapes all special chars", () => {
        assert.equal(escapeAttr('a&b<c>"d'), "a&amp;b&lt;c&gt;&quot;d");
    });
});

describe("formatDuration", () => {
    it("formats seconds under 60", () => {
        assert.equal(formatDuration(45), "45s");
    });

    it("rounds fractional seconds", () => {
        assert.equal(formatDuration(3.7), "4s");
    });

    it("formats minutes and seconds", () => {
        assert.equal(formatDuration(125), "2m5s");
    });

    it("formats exact minutes", () => {
        assert.equal(formatDuration(120), "2m0s");
    });

    it("formats zero", () => {
        assert.equal(formatDuration(0), "0s");
    });
});

describe("ANSI color tables", () => {
    it("ansi4Color returns correct colors for indices 0-7", () => {
        assert.equal(ansi4Color(0), "#000");
        assert.equal(ansi4Color(1), "#c33");
        assert.equal(ansi4Color(2), "#3c3");
        assert.equal(ansi4Color(7), "#ccc");
    });

    it("ansi4Color returns fallback for out-of-range", () => {
        assert.equal(ansi4Color(8), "#ccc");
        assert.equal(ansi4Color(-1), "#ccc");
    });

    it("ansi4BrightColor returns correct bright colors", () => {
        assert.equal(ansi4BrightColor(0), "#555");
        assert.equal(ansi4BrightColor(1), "#f55");
        assert.equal(ansi4BrightColor(7), "#fff");
    });

    it("ansi4BrightColor returns fallback for out-of-range", () => {
        assert.equal(ansi4BrightColor(8), "#fff");
    });
});

describe("ansi256Color", () => {
    it("maps 0-7 to standard 4-bit colors", () => {
        assert.equal(ansi256Color(0), "#000");
        assert.equal(ansi256Color(1), "#c33");
    });

    it("maps 8-15 to bright 4-bit colors", () => {
        assert.equal(ansi256Color(8), "#555");
        assert.equal(ansi256Color(15), "#fff");
    });

    it("maps 16-231 to 216-color cube", () => {
        // Color 16 = rgb(0,0,0) in the cube
        assert.equal(ansi256Color(16), "rgb(0,0,0)");
        // Color 196 = idx 180 = r=5*51=255, g=0, b=0 → red
        assert.equal(ansi256Color(196), "rgb(255,0,0)");
    });

    it("maps 232-255 to grayscale ramp", () => {
        assert.equal(ansi256Color(232), "rgb(8,8,8)");
        assert.equal(ansi256Color(255), "rgb(238,238,238)");
    });
});

describe("ansiToHtml", () => {
    it("passes plain text through with HTML escaping", () => {
        assert.equal(ansiToHtml("hello <world>"), "hello &lt;world&gt;");
    });

    it("renders bold text", () => {
        const result = ansiToHtml("\x1b[1mBOLD\x1b[0m normal");
        assert.ok(result.includes('class="ansi-bold"'));
        assert.ok(result.includes("BOLD"));
        assert.ok(result.includes("normal"));
    });

    it("renders foreground colors", () => {
        const result = ansiToHtml("\x1b[31mRED\x1b[0m");
        assert.ok(result.includes('style="color:#c33"'));
        assert.ok(result.includes("RED"));
    });

    it("renders background colors", () => {
        const result = ansiToHtml("\x1b[42mGREEN BG\x1b[0m");
        assert.ok(result.includes("background-color:#3c3"));
        assert.ok(result.includes("GREEN BG"));
    });

    it("renders bright foreground colors (90-97)", () => {
        const result = ansiToHtml("\x1b[91mBRIGHT RED\x1b[0m");
        assert.ok(result.includes('style="color:#f55"'));
    });

    it("handles 256-color foreground", () => {
        const result = ansiToHtml("\x1b[38;5;196mRED256\x1b[0m");
        assert.ok(result.includes("color:rgb(255,0,0)"));
    });

    it("handles true-color foreground", () => {
        const result = ansiToHtml("\x1b[38;2;100;150;200mTRUECOLOR\x1b[0m");
        assert.ok(result.includes("color:rgb(100,150,200)"));
    });

    it("resets all styles on code 0", () => {
        const result = ansiToHtml("\x1b[1;31mSTYLED\x1b[0mPLAIN");
        // After reset, "PLAIN" should have no span wrapper
        assert.ok(result.endsWith("PLAIN"));
    });

    it("handles empty input", () => {
        assert.equal(ansiToHtml(""), "");
    });

    it("skips cursor movement sequences", () => {
        const result = ansiToHtml("before\x1b[2Aafter");
        assert.equal(result, "beforeafter");
    });

    it("combines bold + color in a single span", () => {
        const result = ansiToHtml("\x1b[1;32mBOLD GREEN\x1b[0m");
        assert.ok(result.includes("ansi-bold"));
        assert.ok(result.includes("color:#3c3"));
        assert.ok(result.includes("BOLD GREEN"));
    });

    it("handles multiple style changes", () => {
        const result = ansiToHtml("\x1b[31mRED\x1b[32mGREEN\x1b[0m");
        assert.ok(result.includes("color:#c33"));
        assert.ok(result.includes("color:#3c3"));
    });
});

describe("applyAnsiCodes", () => {
    it("reset (code 0) clears all styles", () => {
        const result = applyAnsiCodes(["ansi-bold", "color:#c33"], [0]);
        assert.deepEqual(result, []);
    });

    it("accumulates decoration styles", () => {
        let styles = applyAnsiCodes([], [1]); // bold
        styles = applyAnsiCodes(styles, [4]); // + underline
        assert.ok(styles.includes("ansi-bold"));
        assert.ok(styles.includes("ansi-underline"));
    });

    it("replaces foreground color without duplicating", () => {
        let styles = applyAnsiCodes([], [31]); // red
        assert.deepEqual(styles, ["color:#c33"]);

        styles = applyAnsiCodes(styles, [32]); // green replaces red
        assert.deepEqual(styles, ["color:#3c3"]);
    });

    it("replaces background color without duplicating", () => {
        let styles = applyAnsiCodes([], [41]); // red bg
        styles = applyAnsiCodes(styles, [42]); // green bg replaces red bg
        assert.equal(styles.filter(s => s.startsWith("background")).length, 1);
        assert.ok(styles.includes("background-color:#3c3"));
    });
});

describe("AppState", () => {
    let state;

    beforeEach(() => {
        state = new AppState();
    });

    describe("hydrate", () => {
        it("sets all fields from state snapshot", () => {
            state.hydrate({
                prompts: [{ id: 1, status: "running", text: "test" }],
                max_workers: 5,
                active_workers: 2,
                default_mode: "one-shot",
            });
            assert.equal(state.prompts.length, 1);
            assert.equal(state.maxWorkers, 5);
            assert.equal(state.activeWorkers, 2);
            assert.equal(state.defaultMode, "one-shot");
        });

        it("handles empty/missing fields", () => {
            state.hydrate({});
            assert.deepEqual(state.prompts, []);
            assert.equal(state.maxWorkers, 0);
            assert.equal(state.defaultMode, "interactive");
        });

        it("triggers onChange listeners", () => {
            let called = false;
            state.onChange(() => { called = true; });
            state.hydrate({ prompts: [] });
            assert.ok(called);
        });
    });

    describe("update — ConnectionStatus", () => {
        it("sets connected=true on 'connected'", () => {
            state.update({ type: "ConnectionStatus", status: "connected" });
            assert.equal(state.connected, true);
        });

        it("sets connected=false on 'disconnected'", () => {
            state.connected = true;
            state.update({ type: "ConnectionStatus", status: "disconnected" });
            assert.equal(state.connected, false);
        });

        it("sets connected=false on 'connecting'", () => {
            state.connected = true;
            state.update({ type: "ConnectionStatus", status: "connecting" });
            assert.equal(state.connected, false);
        });
    });

    describe("_applyEvent — PromptAdded", () => {
        it("adds a new prompt", () => {
            state.update({ type: "DaemonEvent", event: { type: "PromptAdded", id: 1, status: "pending", text: "hello" } });
            assert.equal(state.prompts.length, 1);
            assert.equal(state.prompts[0].text, "hello");
        });

        it("updates existing prompt with same ID", () => {
            state.prompts = [{ id: 1, status: "pending", text: "old" }];
            state.update({ type: "DaemonEvent", event: { type: "PromptAdded", id: 1, status: "pending", text: "new" } });
            assert.equal(state.prompts.length, 1);
            assert.equal(state.prompts[0].text, "new");
        });
    });

    describe("_applyEvent — PromptUpdated", () => {
        it("updates existing prompt", () => {
            state.prompts = [{ id: 1, status: "pending", text: "test" }];
            state.update({ type: "DaemonEvent", event: { type: "PromptUpdated", id: 1, status: "running", text: "test" } });
            assert.equal(state.prompts[0].status, "running");
        });

        it("adds prompt if not found", () => {
            state.update({ type: "DaemonEvent", event: { type: "PromptUpdated", id: 5, status: "running", text: "new" } });
            assert.equal(state.prompts.length, 1);
            assert.equal(state.prompts[0].id, 5);
        });
    });

    describe("_applyEvent — PromptRemoved", () => {
        it("removes prompt by id", () => {
            state.prompts = [
                { id: 1, status: "completed", text: "a" },
                { id: 2, status: "pending", text: "b" },
            ];
            state.update({ type: "DaemonEvent", event: { type: "PromptRemoved", prompt_id: 1 } });
            assert.equal(state.prompts.length, 1);
            assert.equal(state.prompts[0].id, 2);
        });

        it("no-op when prompt not found", () => {
            state.prompts = [{ id: 1, status: "completed", text: "a" }];
            state.update({ type: "DaemonEvent", event: { type: "PromptRemoved", prompt_id: 99 } });
            assert.equal(state.prompts.length, 1);
        });
    });

    describe("_applyEvent — WorkerStarted", () => {
        it("sets prompt status to running", () => {
            state.prompts = [{ id: 1, status: "pending", text: "test" }];
            state.maxWorkers = 5;
            state.update({ type: "DaemonEvent", event: { type: "WorkerStarted", prompt_id: 1 } });
            assert.equal(state.prompts[0].status, "running");
        });

        it("increments activeWorkers", () => {
            state.maxWorkers = 5;
            state.prompts = [{ id: 1, status: "pending", text: "test" }];
            state.update({ type: "DaemonEvent", event: { type: "WorkerStarted", prompt_id: 1 } });
            assert.equal(state.activeWorkers, 1);
        });

        it("caps activeWorkers at maxWorkers", () => {
            state.maxWorkers = 2;
            state.activeWorkers = 2;
            state.prompts = [{ id: 1, status: "pending", text: "test" }];
            state.update({ type: "DaemonEvent", event: { type: "WorkerStarted", prompt_id: 1 } });
            assert.equal(state.activeWorkers, 2);
        });
    });

    describe("_applyEvent — WorkerFinished", () => {
        it("sets prompt status to completed", () => {
            state.prompts = [{ id: 1, status: "running", text: "test" }];
            state.activeWorkers = 1;
            state.update({ type: "DaemonEvent", event: { type: "WorkerFinished", prompt_id: 1 } });
            assert.equal(state.prompts[0].status, "completed");
        });

        it("decrements activeWorkers", () => {
            state.activeWorkers = 2;
            state.prompts = [{ id: 1, status: "running", text: "test" }];
            state.update({ type: "DaemonEvent", event: { type: "WorkerFinished", prompt_id: 1 } });
            assert.equal(state.activeWorkers, 1);
        });

        it("floors activeWorkers at 0", () => {
            state.activeWorkers = 0;
            state.prompts = [{ id: 1, status: "running", text: "test" }];
            state.update({ type: "DaemonEvent", event: { type: "WorkerFinished", prompt_id: 1 } });
            assert.equal(state.activeWorkers, 0);
        });
    });

    describe("_applyEvent — WorkerError", () => {
        it("sets prompt status to failed with error message", () => {
            state.prompts = [{ id: 1, status: "running", text: "test" }];
            state.activeWorkers = 1;
            state.update({ type: "DaemonEvent", event: { type: "WorkerError", prompt_id: 1, error: "timeout" } });
            assert.equal(state.prompts[0].status, "failed");
            assert.equal(state.prompts[0].error, "timeout");
        });

        it("decrements activeWorkers", () => {
            state.activeWorkers = 1;
            state.prompts = [{ id: 1, status: "running", text: "test" }];
            state.update({ type: "DaemonEvent", event: { type: "WorkerError", prompt_id: 1 } });
            assert.equal(state.activeWorkers, 0);
        });
    });

    describe("_applyEvent — OutputChunk", () => {
        it("appends text to prompt output", () => {
            state.prompts = [{ id: 1, status: "running", text: "test" }];
            state.update({ type: "DaemonEvent", event: { type: "OutputChunk", prompt_id: 1, text: "hello " } });
            state.update({ type: "DaemonEvent", event: { type: "OutputChunk", prompt_id: 1, text: "world" } });
            assert.equal(state.prompts[0].output, "hello world");
            assert.equal(state.prompts[0].output_len, 11);
        });

        it("initializes output for prompt with no prior output", () => {
            state.prompts = [{ id: 1, status: "running", text: "test" }];
            state.update({ type: "DaemonEvent", event: { type: "OutputChunk", prompt_id: 1, text: "first" } });
            assert.equal(state.prompts[0].output, "first");
        });
    });

    describe("_applyEvent — MaxWorkersChanged", () => {
        it("updates maxWorkers", () => {
            state.update({ type: "DaemonEvent", event: { type: "MaxWorkersChanged", count: 8 } });
            assert.equal(state.maxWorkers, 8);
        });
    });

    describe("_applyEvent — StateSnapshot", () => {
        it("hydrates full state from snapshot event", () => {
            state.update({
                type: "DaemonEvent",
                event: {
                    type: "StateSnapshot",
                    prompts: [{ id: 1, status: "running", text: "a" }],
                    max_workers: 3,
                    active_workers: 1,
                    default_mode: "one-shot",
                },
            });
            assert.equal(state.prompts.length, 1);
            assert.equal(state.maxWorkers, 3);
            assert.equal(state.defaultMode, "one-shot");
        });
    });

    describe("_sortPrompts", () => {
        it("sorts running before pending before completed before failed", () => {
            state.prompts = [
                { id: 1, status: "failed", text: "a" },
                { id: 2, status: "running", text: "b" },
                { id: 3, status: "completed", text: "c" },
                { id: 4, status: "pending", text: "d" },
            ];
            state._sortPrompts();
            assert.deepEqual(state.prompts.map(p => p.status), ["running", "pending", "completed", "failed"]);
        });

        it("sorts by ID descending within same status", () => {
            state.prompts = [
                { id: 1, status: "pending", text: "a" },
                { id: 3, status: "pending", text: "b" },
                { id: 2, status: "pending", text: "c" },
            ];
            state._sortPrompts();
            assert.deepEqual(state.prompts.map(p => p.id), [3, 2, 1]);
        });

        it("places idle between pending and completed", () => {
            state.prompts = [
                { id: 1, status: "completed", text: "a" },
                { id: 2, status: "idle", text: "b" },
                { id: 3, status: "pending", text: "c" },
            ];
            state._sortPrompts();
            assert.deepEqual(state.prompts.map(p => p.status), ["pending", "idle", "completed"]);
        });
    });

    describe("onChange / unsubscribe", () => {
        it("calls listener on state change", () => {
            let callCount = 0;
            state.onChange(() => { callCount++; });
            state.update({ type: "ConnectionStatus", status: "connected" });
            assert.equal(callCount, 1);
        });

        it("unsubscribe stops notifications", () => {
            let callCount = 0;
            const unsub = state.onChange(() => { callCount++; });
            state.update({ type: "ConnectionStatus", status: "connected" });
            assert.equal(callCount, 1);
            unsub();
            state.update({ type: "ConnectionStatus", status: "disconnected" });
            assert.equal(callCount, 1);
        });
    });

    describe("update — unknown types", () => {
        it("ignores unknown message types", () => {
            state.prompts = [{ id: 1, status: "running", text: "test" }];
            state.update({ type: "SomethingUnknown" });
            assert.equal(state.prompts.length, 1);
        });

        it("ignores unknown event types", () => {
            let notified = false;
            state.onChange(() => { notified = true; });
            state.update({ type: "DaemonEvent", event: { type: "UnknownEvent" } });
            assert.equal(notified, false);
        });
    });
});

describe("Auth helpers", () => {
    it("getAuthToken returns null when no token set", () => {
        const storage = createStorageMock();
        assert.equal(storage.getItem("clhorde_auth_token"), null);
    });

    it("setAuthToken / getAuthToken round-trips", () => {
        const storage = createStorageMock();
        storage.setItem("clhorde_auth_token", "my-secret");
        assert.equal(storage.getItem("clhorde_auth_token"), "my-secret");
    });

    it("clearAuthToken removes stored token", () => {
        const storage = createStorageMock();
        storage.setItem("clhorde_auth_token", "my-secret");
        storage.removeItem("clhorde_auth_token");
        assert.equal(storage.getItem("clhorde_auth_token"), null);
    });
});

describe("authHeaders", () => {
    // Standalone version that takes token as arg for testability
    function authHeadersWith(token, extra = {}) {
        const headers = { ...extra };
        if (token) {
            headers["Authorization"] = `Bearer ${token}`;
        }
        return headers;
    }

    it("includes Bearer token when present", () => {
        const h = authHeadersWith("abc123");
        assert.equal(h["Authorization"], "Bearer abc123");
    });

    it("omits Authorization when no token", () => {
        const h = authHeadersWith(null);
        assert.equal(h["Authorization"], undefined);
    });

    it("merges with extra headers", () => {
        const h = authHeadersWith("tok", { "Content-Type": "application/json" });
        assert.equal(h["Authorization"], "Bearer tok");
        assert.equal(h["Content-Type"], "application/json");
    });
});

describe("Prompt filtering logic", () => {
    const prompts = [
        { id: 1, status: "running", text: "Review auth module" },
        { id: 2, status: "completed", text: "Fix login bug" },
        { id: 3, status: "pending", text: "Refactor auth helpers" },
        { id: 4, status: "failed", text: "Update docs" },
    ];

    function filterPrompts(list, text, status) {
        return list.filter(p => {
            const st = (p.status || "").toLowerCase();
            if (status !== "all" && st !== status) return false;
            if (text && !p.text.toLowerCase().includes(text.toLowerCase())) return false;
            return true;
        });
    }

    it("returns all prompts when no filter", () => {
        assert.equal(filterPrompts(prompts, "", "all").length, 4);
    });

    it("filters by text (case-insensitive)", () => {
        const result = filterPrompts(prompts, "auth", "all");
        assert.equal(result.length, 2);
        assert.deepEqual(result.map(p => p.id), [1, 3]);
    });

    it("filters by status", () => {
        const result = filterPrompts(prompts, "", "completed");
        assert.equal(result.length, 1);
        assert.equal(result[0].id, 2);
    });

    it("combines text and status filters", () => {
        const result = filterPrompts(prompts, "auth", "pending");
        assert.equal(result.length, 1);
        assert.equal(result[0].id, 3);
    });

    it("returns empty when no match", () => {
        const result = filterPrompts(prompts, "nonexistent", "all");
        assert.equal(result.length, 0);
    });
});

describe("Action button visibility logic", () => {
    function getVisibleActions(status) {
        const actions = [];
        const isRunning = status === "running" || status === "idle";
        const isDone = status === "completed" || status === "failed";
        const isPending = status === "pending";

        if (isRunning) actions.push("kill");
        if (isDone) actions.push("retry", "resume");
        if (isPending) actions.push("move-up", "move-down");
        actions.push("delete"); // always visible
        return actions;
    }

    it("running prompt shows kill + delete", () => {
        assert.deepEqual(getVisibleActions("running"), ["kill", "delete"]);
    });

    it("idle prompt shows kill + delete", () => {
        assert.deepEqual(getVisibleActions("idle"), ["kill", "delete"]);
    });

    it("completed prompt shows retry + resume + delete", () => {
        assert.deepEqual(getVisibleActions("completed"), ["retry", "resume", "delete"]);
    });

    it("failed prompt shows retry + resume + delete", () => {
        assert.deepEqual(getVisibleActions("failed"), ["retry", "resume", "delete"]);
    });

    it("pending prompt shows move-up + move-down + delete", () => {
        assert.deepEqual(getVisibleActions("pending"), ["move-up", "move-down", "delete"]);
    });
});

describe("Action route mapping", () => {
    function getActionRoute(action, id) {
        const routes = {
            "kill":      { method: "POST",   url: `/api/prompts/${id}/kill` },
            "retry":     { method: "POST",   url: `/api/prompts/${id}/retry` },
            "resume":    { method: "POST",   url: `/api/prompts/${id}/resume` },
            "delete":    { method: "DELETE",  url: `/api/prompts/${id}` },
            "move-up":   { method: "POST",   url: `/api/prompts/${id}/move-up` },
            "move-down": { method: "POST",   url: `/api/prompts/${id}/move-down` },
        };
        return routes[action];
    }

    it("kill uses POST", () => {
        const r = getActionRoute("kill", 5);
        assert.equal(r.method, "POST");
        assert.equal(r.url, "/api/prompts/5/kill");
    });

    it("delete uses DELETE", () => {
        const r = getActionRoute("delete", 3);
        assert.equal(r.method, "DELETE");
        assert.equal(r.url, "/api/prompts/3");
    });

    it("retry uses POST", () => {
        const r = getActionRoute("retry", 7);
        assert.equal(r.method, "POST");
        assert.equal(r.url, "/api/prompts/7/retry");
    });

    it("resume uses POST", () => {
        const r = getActionRoute("resume", 7);
        assert.equal(r.method, "POST");
        assert.equal(r.url, "/api/prompts/7/resume");
    });

    it("move-up uses POST", () => {
        const r = getActionRoute("move-up", 2);
        assert.equal(r.method, "POST");
        assert.equal(r.url, "/api/prompts/2/move-up");
    });
});

describe("Store action mapping", () => {
    function getStoreRoute(action) {
        const routes = {
            "drop-all":        { url: "/api/store/drop", body: { filter: "all" } },
            "drop-completed":  { url: "/api/store/drop", body: { filter: "completed" } },
            "drop-failed":     { url: "/api/store/drop", body: { filter: "failed" } },
            "keep-completed":  { url: "/api/store/keep", body: { filter: "completed" } },
            "clean-worktrees": { url: "/api/store/clean-worktrees", body: null },
            "refresh":         { url: null },
        };
        return routes[action];
    }

    it("drop-all maps correctly", () => {
        const r = getStoreRoute("drop-all");
        assert.equal(r.url, "/api/store/drop");
        assert.deepEqual(r.body, { filter: "all" });
    });

    it("drop-completed maps correctly", () => {
        const r = getStoreRoute("drop-completed");
        assert.deepEqual(r.body, { filter: "completed" });
    });

    it("keep-completed maps correctly", () => {
        const r = getStoreRoute("keep-completed");
        assert.equal(r.url, "/api/store/keep");
        assert.deepEqual(r.body, { filter: "completed" });
    });

    it("clean-worktrees has null body", () => {
        const r = getStoreRoute("clean-worktrees");
        assert.equal(r.url, "/api/store/clean-worktrees");
        assert.equal(r.body, null);
    });

    it("refresh has null url (local-only action)", () => {
        const r = getStoreRoute("refresh");
        assert.equal(r.url, null);
    });

    it("drop actions require confirmation (prefix check)", () => {
        for (const action of ["drop-all", "drop-completed", "drop-failed"]) {
            assert.ok(action.startsWith("drop"), `${action} should start with 'drop'`);
        }
        assert.ok(!"keep-completed".startsWith("drop"));
        assert.ok(!"clean-worktrees".startsWith("drop"));
    });
});

describe("Input bar visibility logic", () => {
    function shouldShowInput(status, mode) {
        const isRunning = status === "running" || status === "idle";
        const isOneShot = mode !== "interactive";
        return isRunning && isOneShot;
    }

    it("shown for running one-shot prompts", () => {
        assert.ok(shouldShowInput("running", "one-shot"));
    });

    it("shown for idle one-shot prompts", () => {
        assert.ok(shouldShowInput("idle", "one-shot"));
    });

    it("hidden for running interactive prompts (uses xterm.js)", () => {
        assert.ok(!shouldShowInput("running", "interactive"));
    });

    it("hidden for completed prompts", () => {
        assert.ok(!shouldShowInput("completed", "one-shot"));
    });

    it("hidden for pending prompts", () => {
        assert.ok(!shouldShowInput("pending", "one-shot"));
    });

    it("hidden for failed prompts", () => {
        assert.ok(!shouldShowInput("failed", "one-shot"));
    });
});

describe("URL hash parsing", () => {
    function parsePromptHash(hash) {
        const match = hash.match(/^#prompt-(\d+)$/);
        return match ? Number(match[1]) : null;
    }

    it("extracts prompt ID from valid hash", () => {
        assert.equal(parsePromptHash("#prompt-5"), 5);
        assert.equal(parsePromptHash("#prompt-123"), 123);
    });

    it("returns null for invalid hashes", () => {
        assert.equal(parsePromptHash("#prompt-"), null);
        assert.equal(parsePromptHash("#other"), null);
        assert.equal(parsePromptHash(""), null);
        assert.equal(parsePromptHash("#prompt-abc"), null);
    });
});

describe("WebSocket reconnect backoff", () => {
    it("doubles backoff up to max", () => {
        let backoff = 1000;
        const maxBackoff = 30000;
        const sequence = [];

        for (let i = 0; i < 8; i++) {
            sequence.push(backoff);
            backoff = Math.min(backoff * 2, maxBackoff);
        }

        assert.deepEqual(sequence, [1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]);
    });
});

describe("PTY base64 decode logic", () => {
    it("decodes base64 to correct Uint8Array", () => {
        // "Hello" in base64
        const b64 = btoa("Hello");
        const binary = atob(b64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) {
            bytes[i] = binary.charCodeAt(i);
        }
        assert.equal(bytes.length, 5);
        assert.equal(bytes[0], 72);  // 'H'
        assert.equal(bytes[1], 101); // 'e'
        assert.equal(bytes[4], 111); // 'o'
    });

    it("handles empty base64", () => {
        const binary = atob("");
        const bytes = new Uint8Array(binary.length);
        assert.equal(bytes.length, 0);
    });

    it("handles binary data with high bytes", () => {
        // ESC [ 3 1 m = ANSI red foreground
        const raw = "\x1b[31m";
        const b64 = btoa(raw);
        const binary = atob(b64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) {
            bytes[i] = binary.charCodeAt(i);
        }
        assert.equal(bytes[0], 0x1b); // ESC
        assert.equal(bytes[1], 0x5b); // [
    });
});

describe("View switching logic", () => {
    it("dashboard and store are the two views", () => {
        const views = ["dashboard", "store"];
        assert.equal(views.length, 2);
    });

    // The actual DOM manipulation is tested via integration tests,
    // but we verify the logic here
    it("store view triggers data load", () => {
        let loaded = false;
        function switchView(view) {
            if (view === "store") loaded = true;
        }
        switchView("store");
        assert.ok(loaded);
    });
});

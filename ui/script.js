const events = new EventSource("/events");
events.addEventListener("ping", () => {});

const subsEl = document.querySelector("#subs");
const addForm = document.querySelector("#add-sub-form");
const addError = document.querySelector("#add-sub-error");
const emptyHint = document.querySelector("#empty-hint");
const startupCard = document.querySelector("#startup");
const startupError = document.querySelector("#startup-error");
const logs = new Map();
let shownRun = 0;
let lastEvent = 0;
let activeKey = "";
let agoTimer = 0;
let page = { winfsp: true, running: false, subscriptions: [] };

function node(tag, className, text) {
    const item = document.createElement(tag);
    if (className) item.className = className;
    if (text != null) item.textContent = text;
    return item;
}

function keyOf(sub) {
    return `${sub.server}/${sub.user}`;
}

function unchangedCount(folder) {
    return Number(folder.unchanged) + Number(folder.already);
}

function summaryText(folder) {
    const parts = [];
    if (folder.uploaded) parts.push(`uploaded ${folder.uploaded}`);
    const unchanged = unchangedCount(folder);
    if (unchanged) parts.push(`unchanged ${unchanged}`);
    if (folder.failed) parts.push(`failed ${folder.failed}`);
    if (parts.length) return parts.join(", ");
    return folder.summary || "No finished backup yet.";
}

function chart(folder) {
    const rows = [
        ["Uploaded", folder.uploaded, "uploaded"],
        ["Unchanged", unchangedCount(folder), "unchanged"],
        ["Failed", folder.failed, "failed"],
    ];
    const max = Math.max(1, ...rows.map((row) => row[1]));
    const wrap = node("div", "chart");
    wrap.setAttribute("role", "img");
    wrap.setAttribute("aria-label", rows.map((row) => `${row[0]} ${row[1]}`).join(", "));
    for (const [label, value, kind] of rows) {
        const row = node("div", "bar");
        const track = node("span", "track");
        const fill = node("span", `fill ${kind}`);
        fill.style.width = `${Math.round((value / max) * 100)}%`;
        track.append(fill);
        row.append(node("span", "bar-label", label), track, node("span", "bar-n", String(value)));
        wrap.append(row);
    }
    return wrap;
}

async function readJson(response) {
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error || "The request failed.");
    return body;
}

function ranAtDate(text) {
    const match = /^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})$/.exec(text || "");
    if (!match) return null;
    return new Date(Number(match[1]), Number(match[2]) - 1, Number(match[3]), Number(match[4]), Number(match[5]), Number(match[6]));
}

function ago(date) {
    const seconds = Math.max(0, Math.round((Date.now() - date.getTime()) / 1000));
    if (seconds < 60) return unit(seconds, "second");
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return unit(minutes, "minute");
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return unit(hours, "hour");
    return unit(Math.floor(hours / 24), "day");
}

function unit(count, name) {
    return `${count} ${name}${count === 1 ? "" : "s"} ago`;
}

function paintWhen(element) {
    const time = ranAtDate(element.dataset.ranAt);
    element.textContent = time ? `${element.dataset.ranAt} · ${ago(time)}` : element.dataset.ranAt;
}

function rememberedText(folder) {
    return folder.cached === 1 ? "1 file in the backup cache" : `${folder.cached} files in the backup cache`;
}

function fillStats(card, folder) {
    card.querySelectorAll(":scope > .when, :scope > .chart, :scope > .summary, :scope > .cache").forEach((el) => el.remove());
    const nodes = [];
    if (folder.ranAt) {
        const when = node("p", "when");
        when.dataset.ranAt = folder.ranAt;
        paintWhen(when);
        nodes.push(when);
    }
    nodes.push(chart(folder), node("p", "summary", summaryText(folder)), node("p", "cache", rememberedText(folder)));
    const toggle = card.querySelector(".ignore-toggle");
    if (toggle) toggle.before(...nodes);
    else card.append(...nodes);
}

function ignoreQuery(sub, path) {
    return `server=${encodeURIComponent(sub.server)}&user=${encodeURIComponent(sub.user)}&path=${encodeURIComponent(path)}`;
}

function renderFolder(parent, sub, folder) {
    const card = node("article", "card");
    card.dataset.path = folder.path;
    const head = node("div", "head");
    head.append(node("h2", "", folder.path));
    const remove = node("button", "quiet", "Remove");
    remove.type = "button";
    remove.addEventListener("click", () => removeFolder(sub, folder.path, card));
    head.append(remove);
    card.append(head);
    fillStats(card, folder);

    const toggle = node("button", "ignore-toggle", ".deppoignore");
    toggle.type = "button";
    toggle.setAttribute("aria-expanded", "false");
    const editor = node("div", "ignore-editor");
    editor.hidden = true;
    const area = node("textarea");
    area.spellcheck = false;
    area.dataset.saved = "";
    const actions = node("div", "ignore-actions");
    const save = node("button", "", "Save ignore file");
    save.type = "button";
    const cancel = node("button", "quiet", "Cancel");
    cancel.type = "button";
    cancel.hidden = true;
    const status = node("p", "saved", "");
    const dirty = () => area.value !== area.dataset.saved;
    const paintDirty = () => {
        const changed = dirty();
        cancel.hidden = !changed;
        if (changed) editor.hidden = false;
        toggle.setAttribute("aria-expanded", editor.hidden ? "false" : "true");
        toggle.setAttribute("aria-disabled", changed ? "true" : "false");
    };
    toggle.addEventListener("click", () => {
        if (dirty()) return;
        editor.hidden = !editor.hidden;
        paintDirty();
    });
    area.addEventListener("input", paintDirty);
    const closeEditor = () => {
        editor.hidden = true;
        status.textContent = "";
        paintDirty();
    };
    cancel.addEventListener("click", () => {
        area.value = area.dataset.saved;
        closeEditor();
    });
    save.addEventListener("click", async () => {
        status.textContent = "";
        status.className = "saved";
        try {
            await readJson(await fetch(`/api/ignore?${ignoreQuery(sub, folder.path)}`, {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ text: area.value }),
            }));
            area.dataset.saved = area.value;
            closeEditor();
        } catch (err) {
            status.className = "error";
            status.textContent = err.message;
        }
    });
    actions.append(save, cancel);
    editor.append(area, actions, status);
    card.append(toggle, editor);
    parent.append(card);
    fetch(`/api/ignore?${ignoreQuery(sub, folder.path)}`).then(readJson).then((body) => {
        area.dataset.saved = body.text || "";
        area.value = area.dataset.saved;
        if (!body.exists) status.textContent = "This file is not on disk yet. Saving creates it.";
        paintDirty();
    }).catch((err) => {
        status.className = "error";
        status.textContent = err.message;
    });
}

function timerControl(sub, error) {
    const block = node("div", "timer");
    block.append(node("h3", "", "Automatic backup"));
    const modes = node("div", "timer-modes");
    const group = `timer-${sub.server}-${sub.user}`;
    const off = document.createElement("input");
    const every = document.createElement("input");
    const daily = document.createElement("input");
    for (const input of [off, every, daily]) {
        input.type = "radio";
        input.name = group;
    }
    const interval = document.createElement("input");
    interval.type = "text";
    interval.className = "interval";
    interval.spellcheck = false;
    interval.autocomplete = "off";
    interval.setAttribute("aria-label", "Backup interval");
    interval.value = "6 hours";
    const time = document.createElement("input");
    time.type = "time";
    time.value = "01:00";
    const raw = sub.timer || "";
    if (/^\d+$/.test(raw)) {
        every.checked = true;
        interval.value = formatInterval(Number(raw));
    } else if (/^\d{1,2}:\d{2}$/.test(raw)) {
        daily.checked = true;
        const [hour, minute] = raw.split(":");
        time.value = `${hour.padStart(2, "0")}:${minute}`;
    } else {
        off.checked = true;
    }
    interval.addEventListener("blur", () => {
        const shown = normalizedInterval(interval.value);
        if (shown) {
            interval.value = shown;
            if (every.checked) error.textContent = "";
        }
    });
    const offLabel = node("label");
    offLabel.append(off, document.createTextNode(" Off"));
    const everyLabel = node("label");
    everyLabel.append(every, document.createTextNode(" Every "), interval);
    const dailyLabel = node("label");
    dailyLabel.append(daily, document.createTextNode(" Every day at "), time);
    modes.append(offLabel, everyLabel, dailyLabel);
    const save = node("button", "", "Save automatic backup");
    save.type = "button";
    save.addEventListener("click", async () => {
        error.textContent = "";
        let timer = "";
        if (every.checked) {
            const shown = normalizedInterval(interval.value);
            if (!shown) {
                error.textContent = intervalError(interval.value);
                return;
            }
            interval.value = shown;
            timer = shown;
        } else if (daily.checked) {
            if (!time.value) {
                error.textContent = "Choose a time, in 24-hour form, such as 01:00.";
                return;
            }
            timer = time.value;
        }
        try {
            await readJson(await fetch("/api/timer", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ server: sub.server, user: sub.user, timer }),
            }));
            await load();
        } catch (err) {
            error.textContent = err.message;
        }
    });
    block.append(modes, node("p", "hint", "The interval is at most 24 hours, such as 6 hours, 90 minutes, 30s, or 1:30. A time such as 1:00 or 13:05 runs once a day, in 24-hour time, while Deppo is running."), save);
    return block;
}

function normalizedInterval(raw) {
    try {
        return formatInterval(parseInterval(raw));
    } catch {
        return "";
    }
}

function intervalError(raw) {
    try {
        parseInterval(raw);
        return "";
    } catch (err) {
        return err.message;
    }
}

function parseInterval(raw) {
    const text = String(raw || "").trim();
    const clock = /^(\d+)\s*:\s*(\d+)(?:\s*:\s*(\d+))?$/.exec(text);
    if (clock) {
        const hours = Number(clock[1]);
        const mins = Number(clock[2]);
        const secs = clock[3] == null ? 0 : Number(clock[3]);
        return wholeMinutes(hours * 60 + mins + Math.round(secs / 60));
    }
    const match = /^(\d+(?:\.\d+)?)(?:\s*(.*))?$/.exec(text);
    if (!match) return failInterval();
    const value = Number(match[1]);
    const rest = (match[2] || "").trim();
    if (!rest) {
        if (value >= 1 && value <= 24) return wholeMinutes(Math.round(value * 60));
        if (value > 24 && value <= 1440) return wholeMinutes(Math.round(value));
        return failInterval();
    }
    const unit = rest[0].toLowerCase();
    if (unit === "h") return wholeMinutes(Math.round(value * 60));
    if (unit === "m") return wholeMinutes(Math.round(value));
    if (unit === "s") return wholeMinutes(Math.round(value / 60));
    return failInterval();
}

function wholeMinutes(minutes) {
    if (!Number.isFinite(minutes) || minutes < 1) throw new Error("Automatic backup needs at least 1 minute.");
    if (minutes > 1440) throw new Error("Automatic backup can run at most every 24 hours.");
    return minutes;
}

function failInterval() {
    throw new Error("Use 1 to 24 hours, or up to 1440 minutes. 6 hours, 90m, 30s, and 1:30 all work.");
}

function formatInterval(minutes) {
    if (minutes % 60 === 0) return `${minutes / 60} hours`;
    return `${minutes} minutes`;
}

function renderSub(sub) {
    const card = node("section", "card");
    card.dataset.key = keyOf(sub);
    const head = node("div", "head");
    const title = node("h2", "");
    title.append(document.createTextNode(`${sub.server} · `));
    const link = document.createElement("a");
    link.href = `https://www.deppo.eu/#subscriptions?id=${encodeURIComponent(sub.user)}`;
    link.target = "_blank";
    link.rel = "noopener";
    link.textContent = sub.user;
    title.append(link);
    const remove = node("button", "quiet", "Remove");
    remove.type = "button";
    remove.addEventListener("click", () => removeSub(sub));
    head.append(title, remove);
    card.append(head);
    if (!sub.password) card.append(node("p", "error", "The saved password could not be read. Add this subscription again."));

        const driveError = node("p", "error", "");
    const mountInput = document.createElement("input");
    mountInput.className = "mount";
    mountInput.spellcheck = false;
    mountInput.autocomplete = "off";
    mountInput.placeholder = "~/Deppo";
    mountInput.value = sub.mount || "";
    mountInput.ariaLabel = "Mount path";
    const driveButton = node("button", "", sub.mount ? "Change mount" : "Set mount");
    driveButton.type = "button";
    driveButton.addEventListener("click", async () => {
        driveError.textContent = "";
        driveButton.disabled = true;
        try {
            await readJson(await fetch("/api/mount", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ server: sub.server, user: sub.user, mount: mountInput.value }),
            }));
            await load();
        } catch (err) {
            driveError.textContent = err.message;
            driveButton.disabled = false;
        }
    });
    const driveRow = node("div", "drive-row");
    const driveLabel = node("label");
    driveLabel.append(document.createTextNode("Mount path "), mountInput);
    driveRow.append(driveLabel, driveButton);
    card.append(driveRow, driveError);
    if (sub.mount) card.append(node("p", "hint", `${sub.mount} stays available while Deppo is running in the background.`));

    if (!page.startupDisabled) {
        const timerError = node("p", "error", "");
        card.append(timerControl(sub, timerError), timerError);
    }

    const backup = node("button", "backup", "Back up now");
    backup.type = "button";
    backup.disabled = page.running;
    backup.addEventListener("click", () => backupNow(sub, card));
    const log = node("div", "run-log");
    log.hidden = true;
    const saved = logs.get(keyOf(sub));
    if (saved) {
        log.hidden = false;
        log.append(...saved.map((line) => node("div", line.bad ? "log-line bad" : "log-line", line.text)));
    }
    const runError = node("p", "error", "");
    card.append(backup, log, runError);

    const folders = node("div", "folders");
    if (!sub.folders.length) folders.append(node("p", "hint", "No folders yet. Add one below."));
    for (const folder of sub.folders) renderFolder(folders, sub, folder);
    const form = node("form", "add");
    const path = document.createElement("input");
    path.placeholder = "~/Documents";
    path.autocomplete = "off";
    path.spellcheck = false;
    path.setAttribute("aria-label", "Folder path");
    const add = node("button", "", "Add");
    add.type = "submit";
    const row = node("div", "row");
    row.append(path, add);
    const folderError = node("p", "error", "");
    form.append(node("h3", "", "Add a folder"), row, folderError);
    form.addEventListener("submit", async (event) => {
        event.preventDefault();
        folderError.textContent = "";
        try {
            await readJson(await fetch("/api/folders", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ server: sub.server, user: sub.user, path: path.value }),
            }));
            await load();
        } catch (err) {
            folderError.textContent = err.message;
        }
    });
    card.append(folders, form);
    return card;
}

async function removeFolder(sub, path, card) {
    if (!confirm(`Remove ${path} from this subscription? The folder on disk stays.`)) return;
    const error = card.querySelector(".error");
    try {
        await readJson(await fetch("/api/folders", {
            method: "DELETE",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ server: sub.server, user: sub.user, path }),
        }));
        await load();
    } catch (err) {
        if (error) error.textContent = err.message;
    }
}

async function removeSub(sub) {
    if (!confirm(`Remove subscription ${sub.user} on ${sub.server}?`)) return;
    addError.textContent = "";
    try {
        await readJson(await fetch("/api/subscriptions", {
            method: "DELETE",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ server: sub.server, user: sub.user }),
        }));
        await load();
    } catch (err) {
        addError.textContent = err.message;
    }
}

async function backupNow(sub, card) {
    const error = card.querySelector(".run-log + .error") || card.querySelector(".error");
    if (error) error.textContent = "";
    activeKey = keyOf(sub);
    setRunning(true);
    try {
        await readJson(await fetch("/api/backup", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ server: sub.server, user: sub.user }),
        }));
    } catch (err) {
        setRunning(false);
        if (error) error.textContent = err.message;
    }
}

function setRunning(running) {
    page.running = running;
    for (const button of document.querySelectorAll("button.backup")) {
        button.disabled = running;
        button.textContent = running ? "Backing up…" : "Back up now";
    }
}

async function load() {
    page = await readJson(await fetch("/api/info"));
    startupCard.hidden = !page.startupDisabled;
    emptyHint.hidden = page.subscriptions.length > 0;
    clearInterval(agoTimer);
    subsEl.replaceChildren(...page.subscriptions.map(renderSub));
    agoTimer = setInterval(() => {
        for (const when of document.querySelectorAll(".when")) paintWhen(when);
    }, 1000);
}

function appendLog(row) {
    if (row.id <= lastEvent) return;
    lastEvent = row.id;
    if (row.run !== shownRun) {
        shownRun = row.run;
        logs.set(activeKey, []);
        const current = document.querySelector(`[data-key="${CSS.escape(activeKey)}"] .run-log`);
        if (current) current.replaceChildren();
        setRunning(true);
    }
    const bad = /^(fail |skip |deppo:)/.test(row.text);
    const bucket = logs.get(activeKey) || [];
    bucket.push({ text: row.text, bad });
    while (bucket.length > 2000) bucket.shift();
    logs.set(activeKey, bucket);
    const log = document.querySelector(`[data-key="${CSS.escape(activeKey)}"] .run-log`);
    if (!log) return;
    log.hidden = false;
    log.append(node("div", bad ? "log-line bad" : "log-line", row.text));
    while (log.childElementCount > 2000) log.firstElementChild.remove();
    log.scrollTop = log.scrollHeight;
}

events.addEventListener("log", (ev) => {
    appendLog(JSON.parse(ev.data));
});

events.addEventListener("done", async (ev) => {
    const row = JSON.parse(ev.data);
    if (row.id <= lastEvent) return;
    lastEvent = row.id;
    setRunning(false);
    try {
        await load();
    } catch (err) {
        addError.textContent = err.message;
    }
});

document.querySelector("#startup-enable").addEventListener("click", async () => {
    startupError.textContent = "";
    try {
        await readJson(await fetch("/api/startup", { method: "POST" }));
        await load();
    } catch (err) {
        startupError.textContent = err.message;
    }
});

addForm.server.addEventListener("blur", () => {
    const name = serverName(addForm.server.value);
    if (name) addForm.server.value = name;
});

function serverName(raw) {
    const text = String(raw || "").trim();
    if (!text) return "";
    let host = text;
    if (/^[a-z][a-z0-9+.-]*:\/\//i.test(text)) {
        try {
            host = new URL(text).hostname;
        } catch {
            return text;
        }
    } else {
        host = text.split("/")[0];
        const colon = host.lastIndexOf(":");
        if (colon > 0 && /^\d+$/.test(host.slice(colon + 1))) host = host.slice(0, colon);
    }
    const lower = host.toLowerCase();
    if (lower.endsWith(".deppo.eu")) {
        const name = lower.slice(0, -".deppo.eu".length).split(".").find(Boolean);
        return name || lower;
    }
    return lower.includes(".") ? lower : host;
}

function credentialCombo(text) {
    const match = /^(\d{8}):(.*\S)\s*$/.exec(String(text || "").trim());
    if (!match) return null;
    return { user: match[1], password: match[2].trim() };
}

addForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    addError.textContent = "";
    const server = serverName(addForm.server.value);
    if (server) addForm.server.value = server;
    const credentials = addForm.credentials.value.trim();
    if (!credentialCombo(credentials)) {
        addError.textContent = "Enter credentials as subscription:password.";
        return;
    }
    try {
        await readJson(await fetch("/api/subscriptions", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ server, credentials }),
        }));
        addForm.credentials.value = "";
        await load();
    } catch (err) {
        addError.textContent = err.message;
    }
});

load().catch((err) => {
    addError.textContent = err.message;
});

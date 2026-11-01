const events = new EventSource("/events");
events.addEventListener("ping", () => {});

const screenEl = document.querySelector("#screen");
const pageError = document.querySelector("#page-error");
const startupCard = document.querySelector("#startup");
const startupError = document.querySelector("#startup-error");
const versionEl = document.querySelector("#version");

let page = { version: "", running: false, startupDisabled: false, subscriptions: [] };
let current = null;
let wizard = null;
let logs = new Map();
let shownRun = 0;
let lastEvent = 0;
let activeKey = "";

function node(tag, className, text) {
    const el = document.createElement(tag);
    if (className) el.className = className;
    if (text != null) el.textContent = text;
    return el;
}

function keyOf(sub) {
    return `${sub.server}/${sub.user}`;
}

function findSub(server, user) {
    return page.subscriptions.find((sub) => sub.server === server && sub.user === user) || null;
}

async function readJson(response) {
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error || "The request failed.");
    return body;
}

async function api(path, options) {
    return readJson(await fetch(path, options));
}

function formatBytes(n) {
    const value = Number(n) || 0;
    if (value < 1000) return `${value} B`;
    const units = ["KB", "MB", "GB", "TB"];
    let x = value / 1000;
    let i = 0;
    while (x >= 1000 && i < units.length - 1) {
        x /= 1000;
        i += 1;
    }
    return `${x >= 10 ? x.toFixed(0) : x.toFixed(1)} ${units[i]}`;
}

function timerLabel(raw) {
    const text = String(raw || "").trim();
    if (!text) return "Automatic backup is off.";
    if (/^\d+$/.test(text)) {
        const minutes = Number(text);
        if (minutes % 60 === 0) return `Every ${minutes / 60} hours.`;
        return `Every ${minutes} minutes.`;
    }
    if (/^\d{1,2}:\d{2}$/.test(text)) return `Every day at ${text}.`;
    return text;
}

function serverName(raw) {
    const text = String(raw || "").trim();
    if (!text) return "";
    let host = text;
    if (/^[a-z][a-z0-9+.-]*:\/\//i.test(text)) {
        try { host = new URL(text).hostname; } catch { return text; }
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
    if (!match) throw new Error("Use 1 to 24 hours, or up to 1440 minutes. Examples: 6 hours, 90m.");
    const value = Number(match[1]);
    const rest = (match[2] || "").trim();
    if (!rest) {
        if (value >= 1 && value <= 24) return wholeMinutes(Math.round(value * 60));
        if (value > 24 && value <= 1440) return wholeMinutes(Math.round(value));
        throw new Error("Use 1 to 24 hours, or up to 1440 minutes.");
    }
    const unit = rest[0].toLowerCase();
    if (unit === "h") return wholeMinutes(Math.round(value * 60));
    if (unit === "m") return wholeMinutes(Math.round(value));
    if (unit === "s") return wholeMinutes(Math.round(value / 60));
    throw new Error("Use hours, minutes, or seconds, such as 6 hours or 90 minutes.");
}

function wholeMinutes(minutes) {
    if (!Number.isFinite(minutes) || minutes < 1) throw new Error("Automatic backup needs at least 1 minute.");
    if (minutes > 1440) throw new Error("Automatic backup can run at most every 24 hours.");
    return minutes;
}

function formatInterval(minutes) {
    if (minutes % 60 === 0) return `${minutes / 60} hours`;
    return `${minutes} minutes`;
}

function clearScreen() {
    screenEl.replaceChildren();
    pageError.textContent = "";
}

function actionsBar(...items) {
    const bar = node("div", "actions");
    for (const item of items) {
        if (!item) continue;
        if (item === "spacer") bar.append(node("div", "spacer"));
        else bar.append(item);
    }
    return bar;
}

function fillActions(bar, ...items) {
    bar.replaceChildren();
    for (const item of items) {
        if (!item) continue;
        if (item === "spacer") bar.append(node("div", "spacer"));
        else bar.append(item);
    }
}

function btn(label, className, onClick) {
    const button = node("button", className || "", label);
    button.type = "button";
    if (onClick) button.addEventListener("click", onClick);
    return button;
}

function textLink(label, onClick) {
    const link = node("button", "text-link", label);
    link.type = "button";
    link.addEventListener("click", onClick);
    return link;
}

function setError(el, message) {
    if (el) el.textContent = message || "";
}

async function refreshInfo() {
    page = await api("/api/info");
    versionEl.textContent = page.version ? `Version ${page.version}` : "";
    startupCard.hidden = !page.startupDisabled;
    return page;
}

function goHome() {
    wizard = null;
    const subs = page.subscriptions || [];
    if (!subs.length) {
        showIntro({ firstRun: true });
        return;
    }
    if (subs.length === 1) {
        showOverview(subs[0], { showAdd: true });
        return;
    }
    showSubscriptionList();
}

function startWizard() {
    wizard = { server: "", user: "", mode: "setup" };
    showIntro({ firstRun: !(page.subscriptions || []).length });
}

function showIntro({ firstRun }) {
    current = "intro";
    clearScreen();
    screenEl.append(
        node("h2", "", firstRun ? "Set up Deppo on this computer" : "Add another subscription"),
        node(
            "p",
            "lead",
            "This is the Deppo storage client for Ubuntu. It connects your computer to Deppo immutable storage.",
        ),
        node(
            "p",
            "copy",
            "Next we will check your subscription credentials, optionally set up a drive mount so you can browse Deppo like a folder, and optionally set up automatic backup of folders on this computer.",
        ),
    );
    const credsNote = node("p", "copy creds-note", "");
    credsNote.append(
        document.createTextNode("You will need the server name and credentials from your "),
    );
    const subsLink = document.createElement("a");
    subsLink.className = "text-link";
    subsLink.href = "https://www.deppo.eu/#subscriptions";
    subsLink.target = "_blank";
    subsLink.rel = "noopener";
    subsLink.textContent = "My subscriptions";
    credsNote.append(subsLink);
    credsNote.append(document.createTextNode(" page on the Deppo website."));
    screenEl.append(credsNote);
    const next = btn("Next", "", () => showCredentials());
    const back = firstRun ? null : btn("Back to start", "quiet", goHome);
    screenEl.append(actionsBar(back, "spacer", next));
}

function showCredentials() {
    current = "credentials";
    clearScreen();
    screenEl.append(
        node("h2", "", "Subscription credentials"),
        node("p", "copy", "Enter the Deppo server name and the subscription credentials you received when the subscription was created."),
    );
    const serverLabel = node("label", "", "Server");
    serverLabel.htmlFor = "server";
    const server = document.createElement("input");
    server.id = "server";
    server.autocomplete = "off";
    server.spellcheck = false;
    server.placeholder = "h1";
    server.value = wizard?.server || "";
    const credLabel = node("label", "", "Credentials");
    credLabel.htmlFor = "credentials";
    const credentials = document.createElement("input");
    credentials.id = "credentials";
    credentials.autocomplete = "off";
    credentials.spellcheck = false;
    credentials.placeholder = "12345678:password";
    const error = node("p", "error", "");
    server.addEventListener("blur", () => {
        const name = serverName(server.value);
        if (name) server.value = name;
    });
    const next = btn("Next", "", async () => {
        setError(error, "");
        const name = serverName(server.value);
        if (name) server.value = name;
        if (!name) {
            setError(error, "Enter a server name, such as h1.");
            return;
        }
        const combo = credentialCombo(credentials.value);
        if (!combo) {
            setError(error, "Enter credentials as subscription:password.");
            return;
        }
        next.disabled = true;
        try {
            const saved = await api("/api/subscriptions", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ server: name, credentials: credentials.value.trim() }),
            });
            wizard = { server: saved.server, user: saved.user, mode: "setup" };
            await refreshInfo();
            showMountAsk();
        } catch (err) {
            setError(error, err.message);
            next.disabled = false;
        }
    });
    screenEl.append(serverLabel, server, credLabel, credentials, error, actionsBar(btn("Back", "quiet", () => showIntro({ firstRun: !(page.subscriptions || []).length })), "spacer", next));
    server.focus();
}

function showMountAsk({ editing = false } = {}) {
    current = "mount";
    const sub = findSub(wizard.server, wizard.user);
    clearScreen();
    screenEl.append(
        node("h2", "", editing ? "Edit drive mount" : "Set up a drive mount?"),
        node(
            "p",
            "copy",
            "A drive mount makes this Deppo subscription available as a folder on Ubuntu. You can open, copy, and save files there while Deppo is running.",
        ),
    );
    const pathLabel = node("label", "", "Mount path");
    pathLabel.htmlFor = "mount-path";
    const path = document.createElement("input");
    path.id = "mount-path";
    path.autocomplete = "off";
    path.spellcheck = false;
    path.placeholder = "~/Deppo";
    path.value = sub?.mount || "";
    const error = node("p", "error", "");
    const next = btn(editing ? "Save" : "Next", "", async () => {
        setError(error, "");
        next.disabled = true;
        try {
            await api("/api/mount", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ server: wizard.server, user: wizard.user, mount: path.value }),
            });
            await refreshInfo();
            if (editing) showOverview(findSub(wizard.server, wizard.user), overviewFlags());
            else showBackupAsk();
        } catch (err) {
            setError(error, err.message);
            next.disabled = false;
        }
    });
    const skipOrBack = editing
        ? btn("Cancel", "quiet", () => showOverview(findSub(wizard.server, wizard.user), overviewFlags()))
        : btn("Skip", "quiet", () => showBackupAsk());
    const back = editing ? null : btn("Back", "quiet", showCredentials);
    screenEl.append(pathLabel, path, error, actionsBar(back, skipOrBack, "spacer", next));
}

function showBackupAsk({ editing = false } = {}) {
    current = "backup-ask";
    clearScreen();
    screenEl.append(
        node("h2", "", "Set up automatic backup?"),
        node(
            "p",
            "copy",
            "Automatic backup can copy folders from this computer to Deppo on a schedule you choose. You pick the folders, and you can use ignore rules so temporary or private files are left out.",
        ),
        node(
            "p",
            "copy",
            "Only files that have changed are uploaded. The first backup of a large folder can take a long time; later runs are usually much faster.",
        ),
    );
    const next = btn("Next", "", () => {
        const sub = findSub(wizard.server, wizard.user);
        if (sub?.folders?.length) showFolderList();
        else showFolderAdd({ afterList: false });
    });
    const skip = btn("Skip", "quiet", async () => {
        if (!editing) {
            await api("/api/timer", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ server: wizard.server, user: wizard.user, timer: "" }),
            }).catch(() => {});
            await refreshInfo();
        }
        showOverview(findSub(wizard.server, wizard.user), overviewFlags());
    });
    const back = editing
        ? btn("Cancel", "quiet", () => showOverview(findSub(wizard.server, wizard.user), overviewFlags()))
        : btn("Back", "quiet", () => showMountAsk());
    screenEl.append(actionsBar(back, skip, "spacer", next));
}

function showFolderAdd({ afterList = false, fromOverview = false } = {}) {
    current = "folder-add";
    clearScreen();
    screenEl.append(
        node("h2", "", "Add a backup folder"),
        node("p", "copy", "Choose one folder on this computer to include in automatic backup."),
    );
    const pathLabel = node("label", "", "Folder path");
    pathLabel.htmlFor = "folder-path";
    const path = document.createElement("input");
    path.id = "folder-path";
    path.autocomplete = "off";
    path.spellcheck = false;
    path.placeholder = "~/Documents";
    const error = node("p", "error", "");
    const note = node("div", "");
    const goBack = () => {
        if (fromOverview) showOverview(findSub(wizard.server, wizard.user), overviewFlags());
        else if (afterList) showFolderList();
        else showBackupAsk();
    };
    const doneAdding = () => {
        if (fromOverview) showOverview(findSub(wizard.server, wizard.user), overviewFlags());
        else showFolderList();
    };
    const add = btn("Add folder", "", async () => {
        setError(error, "");
        note.replaceChildren();
        add.disabled = true;
        try {
            const result = await api("/api/folders", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ server: wizard.server, user: wizard.user, path: path.value }),
            });
            await refreshInfo();
            path.value = "";
            note.append(
                node("p", "ok", `Added ${result.path} (${formatBytes(result.bytes)}, ${result.files} files after ignore rules).`),
                node(
                    "p",
                    "copy",
                    result.ignoreCreated
                        ? "Deppo created a .deppoignore file in that folder. It uses the same rules as .gitignore and already includes common exclusions, so you usually do not need to edit it."
                        : "This folder already had a .deppoignore file. It uses the same rules as .gitignore.",
                ),
            );
            const ignoreLink = node("p", "copy", "");
            ignoreLink.append(document.createTextNode("Ignore file: "));
            ignoreLink.append(node("code", "", result.ignorePath));
            note.append(ignoreLink);
            fillActions(
                foot,
                btn("Add another folder", "primary-outline", () => showFolderAdd({ afterList, fromOverview })),
                "spacer",
                btn("Done adding folders", "", doneAdding),
            );
        } catch (err) {
            setError(error, err.message);
            add.disabled = false;
        }
    });
    const foot = actionsBar(btn("Back", "quiet", goBack), "spacer", add);
    screenEl.append(pathLabel, path, error, note, foot);
    path.focus();
}

async function showFolderList() {
    current = "folder-list";
    clearScreen();
    await refreshInfo();
    const sub = findSub(wizard.server, wizard.user);
    screenEl.append(
        node("h2", "", "Backup folders"),
        node("p", "copy", "These folders will be included. Hit Next to choose when automatic backup should run, or Skip to leave automatic backup off."),
    );
    const list = node("ul", "list");
    const error = node("p", "error", "");
    if (!sub?.folders?.length) {
        list.append(node("li", "", "No folders yet."));
    } else {
        for (const folder of sub.folders) {
            const item = node("li", "");
            const title = node("div", "title", folder.path);
            const meta = node("p", "meta", "Counting…");
            const del = textLink("Delete", async () => {
                if (!confirm(`Remove ${folder.path} from backup? The folder on disk stays.`)) return;
                try {
                    await api("/api/folders", {
                        method: "DELETE",
                        headers: { "Content-Type": "application/json" },
                        body: JSON.stringify({ server: wizard.server, user: wizard.user, path: folder.path }),
                    });
                    await showFolderList();
                } catch (err) {
                    setError(error, err.message);
                }
            });
            item.append(title, del, meta);
            list.append(item);
            api(`/api/folder-bytes?server=${encodeURIComponent(wizard.server)}&user=${encodeURIComponent(wizard.user)}&path=${encodeURIComponent(folder.path)}`)
                .then((body) => {
                    meta.textContent = `${formatBytes(body.bytes)} · ${body.files} files (after .deppoignore)`;
                })
                .catch(() => {
                    meta.textContent = "Could not measure size.";
                });
        }
    }
    const next = btn("Next", "", async () => {
        await refreshInfo();
        const latest = findSub(wizard.server, wizard.user);
        if (!latest?.folders?.length) {
            showBackupAsk();
            return;
        }
        showTimer();
    });
    const skip = btn("Skip", "quiet", async () => {
        await api("/api/timer", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ server: wizard.server, user: wizard.user, timer: "" }),
        });
        await refreshInfo();
        showOverview(findSub(wizard.server, wizard.user), overviewFlags());
    });
    screenEl.append(
        list,
        error,
        actionsBar(
            btn("Back", "quiet", () => showFolderAdd({ afterList: true })),
            btn("Add folder", "primary-outline", () => showFolderAdd({ afterList: true })),
            skip,
            "spacer",
            next,
        ),
    );
}

function showTimer({ editing = false } = {}) {
    current = "timer";
    const sub = findSub(wizard.server, wizard.user);
    clearScreen();
    screenEl.append(
        node("h2", "", editing ? "Edit automatic backup time" : "When should backup run?"),
        node(
            "p",
            "copy",
            "You have two options: back up after an interval, or back up at a certain time every day. Backup can run only while this computer is left on and Deppo is running.",
        ),
    );

    const group = `timer-${wizard.server}-${wizard.user}`;
    const every = document.createElement("input");
    every.type = "radio";
    every.name = group;
    const daily = document.createElement("input");
    daily.type = "radio";
    daily.name = group;
    const interval = document.createElement("input");
    interval.type = "text";
    interval.value = "6 hours";
    interval.autocomplete = "off";
    interval.spellcheck = false;
    const time = document.createElement("input");
    time.type = "time";
    time.value = "01:00";
    const raw = sub?.timer || "";
    if (/^\d+$/.test(raw)) {
        every.checked = true;
        interval.value = formatInterval(Number(raw));
    } else if (/^\d{1,2}:\d{2}$/.test(raw)) {
        daily.checked = true;
        const [hour, minute] = raw.split(":");
        time.value = `${hour.padStart(2, "0")}:${minute}`;
    } else {
        every.checked = true;
    }

    const everyBox = node("div", "choice");
    const everyLabel = node("label");
    everyLabel.append(every, document.createTextNode(" Back up after an interval"));
    const everyDetail = node("div", "detail");
    everyDetail.append(interval, node("p", "hint", "Examples: 6 hours, 90 minutes. At most every 24 hours."));
    everyBox.append(everyLabel, everyDetail);

    const dailyBox = node("div", "choice");
    const dailyLabel = node("label");
    dailyLabel.append(daily, document.createTextNode(" Back up at a certain time every day"));
    const dailyDetail = node("div", "detail");
    dailyDetail.append(time, node("p", "hint", "24-hour time, such as 01:00 or 18:30."));
    dailyBox.append(dailyLabel, dailyDetail);

    const error = node("p", "error", "");
    const saveTimer = async (timer) => {
        await api("/api/timer", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ server: wizard.server, user: wizard.user, timer }),
        });
        await refreshInfo();
        showOverview(findSub(wizard.server, wizard.user), overviewFlags());
    };
    const next = btn(editing ? "Save" : "Next", "", async () => {
        setError(error, "");
        try {
            let timer = "";
            if (every.checked) {
                const minutes = parseInterval(interval.value);
                interval.value = formatInterval(minutes);
                timer = String(minutes);
            } else if (daily.checked) {
                if (!time.value) throw new Error("Choose a time, such as 01:00.");
                timer = time.value;
            } else {
                throw new Error("Choose interval or daily backup.");
            }
            next.disabled = true;
            await saveTimer(timer);
        } catch (err) {
            setError(error, err.message);
            next.disabled = false;
        }
    });
    const secondary = editing
        ? btn("Disable", "quiet", async () => {
            secondary.disabled = true;
            try {
                await saveTimer("");
            } catch (err) {
                setError(error, err.message);
                secondary.disabled = false;
            }
        })
        : btn("Skip", "quiet", async () => {
            secondary.disabled = true;
            try {
                await saveTimer("");
            } catch (err) {
                setError(error, err.message);
                secondary.disabled = false;
            }
        });
    const back = editing
        ? btn("Cancel", "quiet", () => showOverview(findSub(wizard.server, wizard.user), overviewFlags()))
        : btn("Back", "quiet", showFolderList);
    screenEl.append(everyBox, dailyBox, error, actionsBar(back, secondary, "spacer", next));
}

function overviewFlags() {
    return { showAdd: (page.subscriptions || []).length === 1 };
}

function showSubscriptionList() {
    current = "list";
    wizard = null;
    clearScreen();
    screenEl.append(
        node("h2", "", "Your subscriptions"),
        node("p", "copy", "Open a subscription to manage its drive mount and automatic backup."),
    );
    const list = node("ul", "list");
    for (const sub of page.subscriptions) {
        const item = node("li", "");
        const link = textLink(`${sub.server} · ${sub.user}`, () => {
            wizard = { server: sub.server, user: sub.user, mode: "edit" };
            showOverview(sub, { showAdd: false });
        });
        link.className = "title text-link";
        const meta = node("p", "meta", statsLine(sub));
        item.append(link, meta);
        list.append(item);
    }
    screenEl.append(
        list,
        actionsBar("spacer", textLink("Add subscription", startWizard)),
        node("p", "nav-note", "You can open this page any time with deppo --ui."),
    );
}

function ranAtDate(text) {
    const match = /^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})$/.exec(text || "");
    if (!match) return null;
    return new Date(
        Number(match[1]),
        Number(match[2]) - 1,
        Number(match[3]),
        Number(match[4]),
        Number(match[5]),
        Number(match[6]),
    );
}

function startOfDay(date) {
    return new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
}

function ago(date) {
    const seconds = Math.max(0, (Date.now() - date.getTime()) / 1000);
    if (seconds < 30) return "now";
    const dayDiff = Math.round((startOfDay(new Date()) - startOfDay(date)) / 86400000);
    if (dayDiff === 1) return "yesterday";
    if (dayDiff >= 2) return unit(dayDiff, "day");
    const minutes = Math.round(seconds / 60);
    if (minutes < 60) return unit(Math.max(1, minutes), "minute");
    return unit(Math.max(1, Math.round(seconds / 3600)), "hour");
}

function unit(count, name) {
    return `${count} ${name}${count === 1 ? "" : "s"} ago`;
}

function statsLine(sub) {
    const files = Number(sub.filesBackedUp) || 0;
    const fileText = `${files} file${files === 1 ? "" : "s"} backed up`;
    const when = ranAtDate(sub.lastBackup);
    if (when) return `Last backup ${ago(when)} · ${fileText}`;
    if (files > 0) return fileText;
    return "No backup yet";
}

function showOverview(sub, { showAdd }) {
    if (!sub) {
        goHome();
        return;
    }
    current = "overview";
    wizard = { server: sub.server, user: sub.user, mode: wizard?.mode || "edit" };
    clearScreen();
    const head = node("div", "overview-head");
    head.append(node("h2", "overview-title", sub.user));
    head.append(node("span", "overview-server", sub.server));
    const remove = textLink("Remove", async () => {
        if (!confirm(`Remove subscription ${sub.user} on ${sub.server} from this computer?`)) return;
        await api("/api/subscriptions", {
            method: "DELETE",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ server: sub.server, user: sub.user }),
        });
        await refreshInfo();
        goHome();
    });
    remove.classList.add("overview-remove");
    head.append(remove);
    screenEl.append(head, node("p", "stats", statsLine(sub)));

    const mountSection = node("div", "section");
    mountSection.append(node("h3", "section-title", "Drive mount"));
    if (sub.mount) {
        const row = node("div", "inline-row");
        row.append(node("span", "path", sub.mount));
        const links = node("span", "links");
        links.append(
            textLink("Edit", () => showMountAsk({ editing: true })),
            textLink("Delete", async () => {
                if (!confirm("Remove the drive mount for this subscription?")) return;
                await api("/api/mount", {
                    method: "DELETE",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({ server: sub.server, user: sub.user }),
                });
                await refreshInfo();
                showOverview(findSub(sub.server, sub.user), { showAdd });
            }),
        );
        row.append(links);
        mountSection.append(row);
    } else {
        const row = node("div", "inline-row");
        row.append(node("span", "path", "No drive mount"));
        row.append(textLink("Set up a drive mount", () => showMountAsk({ editing: true })));
        mountSection.append(row);
    }
    screenEl.append(mountSection);

    const backupSection = node("div", "section");
    backupSection.append(node("h3", "section-title", "Automatic backup"));
    const stack = node("div", "stack");

    const timerRow = node("div", "stack-row");
    timerRow.append(node("span", "path", timerLabel(sub.timer)));
    const timerLinks = node("span", "links");
    if (sub.timer) {
        timerLinks.append(
            textLink("Edit", () => {
                if (!sub.folders.length) showBackupAsk({ editing: true });
                else showTimer({ editing: true });
            }),
        );
    } else {
        timerLinks.append(textLink("Set up automatic backup", () => showBackupAsk({ editing: true })));
    }
    timerRow.append(timerLinks);
    stack.append(timerRow);

    if (sub.folders.length) {
        for (const folder of sub.folders) {
            const item = node("div", "stack-row");
            item.append(node("span", "folder-path", folder.path));
            item.append(
                textLink("Delete", async () => {
                    if (!confirm(`Remove ${folder.path} from backup? The folder on disk stays.`)) return;
                    await api("/api/folders", {
                        method: "DELETE",
                        headers: { "Content-Type": "application/json" },
                        body: JSON.stringify({ server: sub.server, user: sub.user, path: folder.path }),
                    });
                    await refreshInfo();
                    showOverview(findSub(sub.server, sub.user), { showAdd });
                }),
            );
            stack.append(item);
        }
    } else {
        stack.append(node("div", "stack-row", "No backup folders yet."));
    }
    const addRow = node("div", "stack-row add-folder");
    addRow.append(textLink("Add a folder", () => showFolderAdd({ fromOverview: true })));
    stack.append(addRow);
    backupSection.append(stack);
    screenEl.append(backupSection);

    const log = node("div", "run-log");
    log.hidden = true;
    const saved = logs.get(keyOf(sub));
    if (saved?.length) {
        log.hidden = false;
        for (const line of saved) log.append(node("div", line.bad ? "log-line bad" : "log-line", line.text));
    }
    const runError = node("p", "error", "");
    const backupNow = btn(page.running ? "Backing up…" : "Backup now", "", () => runBackup(sub, runError));
    backupNow.classList.add("backup");
    backupNow.disabled = page.running || !sub.folders.length;
    const close = btn("Close", "quiet", () => {
        window.close();
        pageError.textContent = "You can close this browser tab. Deppo keeps running in the background.";
    });
    const bottomLeft = [];
    if ((page.subscriptions || []).length > 1) {
        bottomLeft.push(btn("All subscriptions", "quiet", () => {
            refreshInfo().then(showSubscriptionList);
        }));
    }
    if (showAdd) bottomLeft.push(textLink("Add subscription", startWizard));
    screenEl.append(actionsBar(...bottomLeft, "spacer", backupNow, close), log, runError);
}

async function runBackup(sub, errorEl) {
    setError(errorEl, "");
    if (!sub.folders.length) {
        setError(errorEl, "Add a folder before backing up.");
        return;
    }
    activeKey = keyOf(sub);
    setRunning(true);
    try {
        await api("/api/backup", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ server: sub.server, user: sub.user }),
        });
    } catch (err) {
        setRunning(false);
        setError(errorEl, err.message);
    }
}

function setRunning(running) {
    page.running = running;
    for (const button of document.querySelectorAll("button.backup")) {
        button.disabled = running;
        button.textContent = running ? "Backing up…" : "Backup now";
    }
}

function appendLog(row) {
    if (row.id <= lastEvent) return;
    lastEvent = row.id;
    if (row.run !== shownRun) {
        shownRun = row.run;
        logs.set(activeKey, []);
        const currentLog = document.querySelector(".run-log");
        if (currentLog) currentLog.replaceChildren();
        setRunning(true);
    }
    const bad = /^(fail |skip |deppo:)/.test(row.text);
    const bucket = logs.get(activeKey) || [];
    bucket.push({ text: row.text, bad });
    while (bucket.length > 2000) bucket.shift();
    logs.set(activeKey, bucket);
    const log = document.querySelector(".run-log");
    if (!log) return;
    log.hidden = false;
    log.append(node("div", bad ? "log-line bad" : "log-line", row.text));
    while (log.childElementCount > 2000) log.firstElementChild.remove();
    log.scrollTop = log.scrollHeight;
}

events.addEventListener("log", (ev) => appendLog(JSON.parse(ev.data)));
events.addEventListener("done", async (ev) => {
    const row = JSON.parse(ev.data);
    if (row.id <= lastEvent) return;
    lastEvent = row.id;
    setRunning(false);
    try {
        await refreshInfo();
        if (current === "overview" && wizard) {
            showOverview(findSub(wizard.server, wizard.user), overviewFlags());
        }
    } catch (err) {
        pageError.textContent = err.message;
    }
});

document.querySelector("#startup-enable").addEventListener("click", async () => {
    startupError.textContent = "";
    try {
        await api("/api/startup", { method: "POST" });
        await refreshInfo();
    } catch (err) {
        startupError.textContent = err.message;
    }
});

refreshInfo()
    .then(goHome)
    .catch((err) => {
        pageError.textContent = err.message;
    });

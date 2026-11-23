// emdx web UI: plain JS over the /api routes of `emdx serve`.
"use strict";

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const esc = (v) =>
  String(v ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const enc = encodeURIComponent;

const state = {
  displays: [],
  selected: null,
  status: {},     // name -> status JSON | {error}
  bluetooth: false,
};

// ─── HTTP ────────────────────────────────────────────────────────────────────

async function api(method, path, body) {
  const opts = { method, headers: {} };
  if (body instanceof FormData) opts.body = body;
  else if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const res = await fetch(path, opts);
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    const err = new Error(data.error || `${res.status} ${res.statusText}`);
    err.reason = data.reason; // "blocked", "wrong-pin": reachable, but refusing the PIN
    throw err;
  }
  return data;
}

function toast(msg, kind = "") {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.textContent = msg;
  $("#toasts").append(el);
  setTimeout(() => el.remove(), kind === "error" ? 7000 : 3500);
}

/** Run `fn` with `btn` showing a spinner; toast the result. */
async function busy(btn, fn, { quiet = false } = {}) {
  if (btn) { btn.disabled = true; btn.classList.add("busy"); }
  try {
    const r = await fn();
    if (!quiet && r && r.message) toast(r.message);
    return r;
  } catch (e) {
    toast(e.message, "error");
    throw e;
  } finally {
    if (btn) { btn.disabled = false; btn.classList.remove("busy"); }
  }
}

const SKIP_DEEP_SLEEP_WARNING = "emdx.skipDeepSleepWarning";
const DEEP_SLEEP_KEY = (name) => `emdx.deepSleep.${name}`;

/** Whether this browser put the display in deep sleep (cleared once it answers again) */
function inDeepSleep(name) { return localStorageGet(DEEP_SLEEP_KEY(name)) === "1"; }
function setDeepSleep(name, on) {
  try { on ? localStorage.setItem(DEEP_SLEEP_KEY(name), "1") : localStorage.removeItem(DEEP_SLEEP_KEY(name)); } catch { /* private mode */ }
}

/**
 * Deep sleep can't be undone over the network: say so, unless this browser
 * was told not to ask again.
 */
function confirmDeepSleep(name) {
  if (localStorageGet(SKIP_DEEP_SLEEP_WARNING) === "1") return Promise.resolve(true);
  const dlg = $("#dlg-deep-sleep"), form = $("#form-deep-sleep");
  form.reset();
  $("#deep-sleep-title").textContent = `Put ${name} in deep sleep?`;
  return new Promise((resolve) => {
    dlg.onclose = () => {
      const ok = dlg.returnValue === "ok";
      if (ok && form.elements.skip.checked) localStorageSet(SKIP_DEEP_SLEEP_WARNING, "1");
      resolve(ok);
    };
    dlg.returnValue = "";
    dlg.showModal();
  });
}

function confirmDialog(title, text, okLabel = "Confirm") {
  const dlg = $("#dlg-confirm");
  $("#confirm-title").textContent = title;
  $("#confirm-text").textContent = text;
  $("#confirm-ok").textContent = okLabel;
  dlg.returnValue = "";
  dlg.showModal();
  return new Promise((resolve) => dlg.addEventListener("close", () => resolve(dlg.returnValue === "ok"), { once: true }));
}

// ─── Display list ────────────────────────────────────────────────────────────

async function loadDisplays(select) {
  state.displays = await api("GET", "/api/displays");
  const names = state.displays.map((d) => d.name);
  let pick = select ?? state.selected ?? localStorageGet("emdx.selected");
  if (!names.includes(pick)) pick = (state.displays.find((d) => d.default) || state.displays[0] || {}).name ?? null;
  renderList();
  if (pick) selectDisplay(pick);
  else renderEmpty();
}

function renderList() {
  const ul = $("#display-list");
  ul.innerHTML = state.displays
    .map((d) => {
      const s = state.status[d.name];
      const dot = !s ? "" : s.loading ? "wait" : s.reason ? "warn" : s.error ? "bad" : "ok";
      return `<li class="display-item ${d.name === state.selected ? "active" : ""}" data-name="${esc(d.name)}" tabindex="0">
        <span class="n">${esc(d.name)}</span>
        <span>${d.default ? '<span class="badge">default</span>' : ""} <span class="dot ${dot}"></span></span>
        <span class="h">${esc(d.host)}</span>
      </li>`;
    })
    .join("");
  $("#empty-list").hidden = state.displays.length > 0;
  $$(".display-item", ul).forEach((li) => {
    li.onclick = () => selectDisplay(li.dataset.name);
    li.onkeydown = (e) => { if (e.key === "Enter") selectDisplay(li.dataset.name); };
  });
}

function renderEmpty() {
  state.selected = null;
  $("#detail").innerHTML = `<div class="placeholder-panel"><div class="notice">
    No display yet. ${state.bluetooth ? "Use <strong>Set up a new display</strong> for a new or factory-reset one, or" : "Use"}
    <strong>Add an already set up display</strong> for one that is already on your network.</div></div>`;
}

function localStorageGet(k) { try { return localStorage.getItem(k); } catch { return null; } }
function localStorageSet(k, v) { try { localStorage.setItem(k, v); } catch { /* private mode */ } }

// ─── Detail ──────────────────────────────────────────────────────────────────

function display(name) { return state.displays.find((d) => d.name === name); }

// Briefly outline a card green to confirm an action went through
function flashCard(sel) {
  const el = $(sel);
  if (!el) return;
  el.classList.remove("flash-ok");
  void el.offsetWidth; // restart the animation if it's still running
  el.classList.add("flash-ok");
  setTimeout(() => el.classList.remove("flash-ok"), 4500);
}

// In the slideshow editor, replace the form with a "Slideshow sent" confirmation
function showSlideshowSent(name, n, every, time) {
  const dlg = $("#dlg-slideshow");
  if (!dlg?.open) return;
  $("#slideshow-sent-detail").textContent =
    `${n} image${n === 1 ? "" : "s"}, each shown for ${every}. Sent at ${time}; the display now runs it on its own.`;
  dlg.querySelector("form").hidden = true;
  $("#slideshow-sent").hidden = false;
}

// Keep the detail header's address in sync without rebuilding the whole panel
function updateDetailHost() {
  const d = display(state.selected);
  const el = $("#detail .d-sub .mono");
  if (d && el) el.textContent = d.host;
}

function selectDisplay(name) {
  state.selected = name;
  localStorageSet("emdx.selected", name);
  renderList();
  renderDetail();
  // Status first, then ask for a fresh thumbnail (one MDC session at a time per display)
  refreshStatus(name).then(() => {
    if (!state.status[name]?.error) askThumb(name, { quiet: true });
  });
}

async function refreshStatus(name) {
  state.status[name] = { ...(state.status[name] || {}), loading: true };
  renderList();
  if (state.selected === name) renderStatusParts();
  try {
    state.status[name] = await api("GET", `/api/displays/${enc(name)}/status`);
    // The display may have been found at a new address while connecting
    const d = display(name);
    if (d && state.status[name].host && state.status[name].host !== d.host) {
      state.displays = await api("GET", "/api/displays");
    }
  } catch (e) {
    state.status[name] = { error: e.message, reason: e.reason };
  }
  renderList();
  if (state.selected === name) { renderStatusParts(); updateDetailHost(); }
}

const PLAYERS = [
  ["mobile", "Mobile (images from emdx)"],
  ["usb", "USB stick"],
  ["vxt", "VXT"],
  ["custom-app", "Custom app"],
];
const ORIENTATIONS = [
  ["landscape", "Landscape"],
  ["portrait", "Portrait"],
];
// The six the display knows, with the colours the phone app shows for them (its own
// swatches, not a promise about what the panel draws)
const BACKGROUNDS = [
  ["white", "White"],
  ["black", "Black"],
  ["red", "Red"],
  ["green", "Green"],
  ["blue", "Blue"],
  ["yellow", "Yellow"],
];
const BACKGROUND_SWATCH = {
  white: "#ffffff", black: "#121212", red: "#eb3333",
  green: "#2e9d58", blue: "#1d8cc8", yellow: "#ffd700",
};
const SLEEP = [["off", "Never"], ["5", "5 minutes"], ["10", "10 minutes"], ["20", "20 minutes"], ["30", "30 minutes"], ["60", "60 minutes"]];
/** name -> Sleep after being edited: last value sent, what is typed, pending send */
const sleepEdits = {};
const SLEEP_MODES = [
  ["sleep", "Sleep mode"],
  ["deep-sleep", "Deep sleep mode"],
  ["off", "Sleep mode off"],
];
const SLEEP_MODE_HELP = {
  sleep: "Sleeps after the time below; Wake-on-LAN wakes it.",
  "deep-sleep": "Sleeps after the time below; only its power button wakes it, not the network.",
  off: "Never sleeps (uses more battery).",
};

function renderDetail() {
  const d = display(state.selected);
  if (!d) return renderEmpty();
  $("#detail").innerHTML = `
    <div class="d-head">
      <div>
        <h1 class="d-title">${esc(d.name)}</h1>
        <div class="d-sub">
          <span class="mono">${esc(d.host)}</span>
          <span id="d-state" class="state"></span>
          <span id="d-battery"></span>
        </div>
      </div>
      <div class="quick">
        <button class="btn" id="a-refresh" title="Read the display's status again">Refresh status</button>
        <button class="btn" id="a-wake" ${d.mac ? "" : "disabled title='No MAC address saved'"}>Wake</button>
        <button class="btn" id="a-sleep" title="Only its power button wakes it again">Deep sleep</button>
        <button class="btn" id="a-identify" title="Show this display's name and details on it">Identify</button>
        <button class="btn" id="a-network" title="Name, IP address, PIN and MAC address emdx uses to reach it">EMDX Settings</button>
      </div>
    </div>

    <div id="d-notice"></div>
    <div class="grid">
      <section class="card">
        <h3>🖼️ Show an image</h3>
        <p class="desc">Any image format; it is converted and sent as is. The content player is switched to Mobile if needed.</p>
        <label class="drop" id="drop">
          <input type="file" accept="image/*" id="file" hidden>
          <span id="drop-preview"></span>
          <span><strong>Drop an image</strong> or click to choose one</span>
        </label>
        <p class="upload-status" id="upload-status" aria-live="polite" hidden></p>
      </section>

      <section class="card">
        <h3>👀 What is on the screen</h3>
        <p class="desc">The display sends an image of what it is showing. A portrait screen is turned upright.</p>
        <div id="thumb" class="thumb"><p class="muted small">No thumbnail yet.</p></div>
        <div class="actions">
          <button class="btn ghost" id="t-ask" title="Ask the display to upload a thumbnail of its current screen">Ask the display</button>
        </div>
      </section>

      <section class="card span" id="card-slideshow">
        <h3>🎞️ Slideshow</h3>
        <p class="desc">The display shows these images in turn, in this order, and keeps going on its own.</p>
        <div id="slide-summary"></div>
        <div class="actions">
          <button class="btn ghost" id="slide-edit">Edit slideshow…</button>
          <button class="btn accent" id="slide-send">Send slideshow</button>
        </div>
        <p class="upload-status" id="slide-status" aria-live="polite" hidden></p>
      </section>

      <section class="card span">
        <h3>🗓️ Schedule</h3>
        <p class="desc">The display changes image at these times, every day, on its own. An image stays until the next time.</p>
        <div class="sched" id="sched"></div>
        <div class="slide-controls">
          <button type="button" class="btn ghost" id="sched-add">Add a time</button>
          <span class="grow"></span>
          <button class="btn accent" id="sched-send">Send schedule</button>
        </div>
        <p class="upload-status" id="sched-status" aria-live="polite" hidden></p>
      </section>

      <section class="card">
        <h3>⚙️ Display settings</h3>
        <p class="desc">Stored on the display itself.</p>
        <div id="settings"><p class="muted small">Reading…</p></div>
      </section>

      <section class="card">
        <h3>ℹ️ About this display</h3>
        <div id="about"><p class="muted small">Reading…</p></div>
      </section>

    </div>`;

  wireDetail(d);
  renderStatusParts();
}

function wireDetail(d) {
  const base = `/api/displays/${enc(d.name)}`;
  $("#a-refresh").onclick = () => refreshStatus(d.name);
  $("#a-wake").onclick = (e) =>
    busy(e.currentTarget, () => api("POST", `${base}/wake`)).then(() => setTimeout(() => refreshStatus(d.name), 8000));
  $("#a-sleep").onclick = async (e) => {
    const btn = e.currentTarget;
    if (!(await confirmDeepSleep(d.name))) return;
    busy(btn, () => api("POST", `${base}/sleep`))
      .then(() => { setDeepSleep(d.name, true); return refreshStatus(d.name); })
      .catch(() => {});
  };
  $("#a-identify").onclick = (e) => busy(e.currentTarget, () => api("POST", `${base}/identify`));

  wireSlideshow(d);
  wireSchedule(d);

  // Image upload
  const drop = $("#drop"), file = $("#file");
  const send = async (f) => {
    if (!f || !f.type.startsWith("image/")) return toast("Choose an image file", "error");
    $("#drop-preview").innerHTML = `<img alt="" src="${URL.createObjectURL(f)}">`;
    const fd = new FormData();
    fd.append("image", f, f.name);
    // What is on the screen is about to change: the display posts the new one when done
    waitForNewThumb(d.name, "Sending the new image; the display posts its thumbnail once it has redrawn…");
    setUpload(d.name, "sending");
    try {
      // Answers once the display has downloaded the image
      await busy(null, () => api("POST", `${base}/image`, fd), { quiet: true });
      // Unless its thumbnail already came in while the request was finishing
      if (uploads[d.name]?.phase === "sending") setUpload(d.name, "sent");
      // The content player was switched to Mobile on the way: show it
      if (state.status[d.name]?.settings?.contentPlayer !== "mobile") refreshStatus(d.name);
    } catch (e) {
      setUpload(d.name, "failed", e.message);
      cancelThumbWait(d.name); // nothing changes on the screen
    }
  };
  renderUpload(d.name);
  file.onchange = () => { send(file.files[0]); file.value = ""; };
  drop.ondragover = (e) => { e.preventDefault(); drop.classList.add("over"); };
  drop.ondragleave = () => drop.classList.remove("over");
  drop.ondrop = (e) => { e.preventDefault(); drop.classList.remove("over"); send(e.dataTransfer.files[0]); };

  // How emdx reaches it, in a dialog
  $("#a-network").onclick = () => openNetwork(d);

  // What the display is showing
  $("#t-ask").onclick = () => askThumb(d.name);
  refreshThumb(d.name);

}

function renderStatusParts() {
  const d = display(state.selected);
  if (!d || !$("#settings")) return;
  const s = state.status[d.name] || {};
  const stateEl = $("#d-state");
  if (s.loading) stateEl.innerHTML = '<span class="dot wait"></span> Checking…';
  else if (s.reason === "blocked") stateEl.innerHTML = '<span class="dot warn"></span> Blocked (too many wrong PINs)';
  else if (s.reason === "wrong-pin") stateEl.innerHTML = '<span class="dot warn"></span> Wrong PIN';
  else if (s.error) stateEl.innerHTML = '<span class="dot bad"></span> Unreachable';
  else if (s.host) stateEl.innerHTML = '<span class="dot ok"></span> Reachable';
  const b = s.battery;
  $("#d-battery").innerHTML = b
    ? `<span class="battery" title="${b.charging ? "Charging" : "Battery"}"><i style="--lvl:${Math.max(0, Math.min(100, b.level)) * 0.2}px"></i>${b.level}%${b.charging ? " ⚡" : ""}</span>`
    : "";

  // Disabled until a status read reaches the display: only Refresh, Wake and
  // EMDX Settings work without it. A refresh in progress keeps the last outcome.
  const down = !(s.host && !s.error);
  $$("#detail .grid > .card").forEach((c) => { c.classList.toggle("disabled", down); c.inert = down; });
  for (const id of ["#a-sleep", "#a-identify"]) $(id).disabled = down;
  if (!down) setDeepSleep(d.name, false);
  const advice = inDeepSleep(d.name)
    ? `It was put in <strong>deep sleep</strong>: only its <strong>power button</strong> wakes it
      (Wake-on-LAN can't). Press it, then <strong>Refresh status</strong>.`
    : `It may be asleep: try <strong>Wake</strong>${d.mac ? "" : " (needs a MAC address)"}, then
      <strong>Refresh status</strong>. If it is in <strong>deep sleep</strong>, only its
      <strong>power button</strong> wakes it.`;
  // Reachable but refusing the PIN: waking it won't help, and each try counts
  const refused = {
    blocked: `${esc(d.name)} answers, but refuses logins after too many wrong PINs. Wait a few
      minutes before <strong>Refresh status</strong>: every try with a wrong PIN counts again.
      Check the saved PIN in <strong>EMDX Settings</strong> first.`,
    "wrong-pin": `${esc(d.name)} answers, but refuses the saved PIN. Fix it in
      <strong>EMDX Settings</strong>, then <strong>Refresh status</strong>; after about 5 wrong
      PINs the display blocks logins for a while.`,
  }[s.reason];
  $("#d-notice").innerHTML = !s.error ? ""
    : refused ? `<div class="notice warn">${refused}</div>`
    : `<div class="notice warn">Couldn't reach ${esc(d.name)}: ${esc(s.error)}.<br>${advice}</div>`;
  if (down) {
    if (s.error) {
      $("#settings").innerHTML = '<p class="muted small">Unavailable while the display is unreachable.</p>';
      $("#about").innerHTML = '<p class="muted small">Unavailable while the display is unreachable.</p>';
    }
    return;
  }
  if (!s.settings) return; // still loading the first time

  const st = s.settings;
  const opt = (list, cur) => list.map(([v, l]) => `<option value="${v}" ${v === cur ? "selected" : ""}>${esc(l)}</option>`).join("");
  const unknown = (v, list) => (v && !list.some(([k]) => k === v) ? `<option value="${esc(v)}" selected>${esc(v)}</option>` : "");
  $("#settings").innerHTML = `
    <div class="setting">
      <span class="label">Content player</span>
      <span class="help">What it shows. Images sent by emdx need “Mobile”.</span>
      <span class="control"><select data-key="contentPlayer">${unknown(st.contentPlayer, PLAYERS)}${opt(PLAYERS, st.contentPlayer)}</select></span>
    </div>
    <div class="setting">
      <span class="label">Orientation</span>
      <span class="help">How the panel is mounted.</span>
      <span class="control"><select data-key="orientation">${opt(ORIENTATIONS, st.orientation)}</select></span>
    </div>
    <div class="setting stacked">
      <span class="label">Background colour</span>
      <span class="help">The display's background colour, shown behind the picture when the picture doesn't fill the whole screen.</span>
      <span class="control"><span class="bg-swatches" role="radiogroup" aria-label="Background colour">${BACKGROUNDS.map(([val, label]) => `<button type="button" class="bg-chip${st.backgroundColor === val ? " selected" : ""}" data-bg="${val}" role="radio" aria-checked="${st.backgroundColor === val}" title="${label}" aria-label="${label}" style="--chip:${BACKGROUND_SWATCH[val]}"></button>`).join("")}</span></span>
    </div>
    <div class="setting">
      <span class="label">Sleep mode</span>
      <span class="help">${esc(SLEEP_MODE_HELP[st.sleepMode] ?? "How it saves energy when not in use.")}${s.battery?.powerSource === "none"
        ? '<span class="hint">To turn off sleep mode, connect the device to a power cord. If not connected, it will enter sleep mode after 3 hours.</span>' : ""}</span>
      <span class="control"><select data-key="sleepMode">${unknown(st.sleepMode, SLEEP_MODES)}${SLEEP_MODES.map(([v, l]) => {
        // Sleep mode off needs mains power (the app's rule)
        const needsPower = v === "off" && s.battery?.powerSource === "none";
        return `<option value="${v}" ${v === st.sleepMode ? "selected" : ""} ${needsPower ? "disabled" : ""}>${esc(needsPower ? `${l} (plug it in first)` : l)}</option>`;
      }).join("")}</select></span>
    </div>
    <div class="setting ${st.sleepMode === "off" ? "off" : ""}">
      <span class="label">Sleep after</span>
      <span class="help">${st.sleepMode === "off" ? "Not used while sleep mode is off." : "Stays awake this long after new content, then sleeps (5 min to 23 h 59 min)."}
        <span class="hint" id="sleep-warn" hidden>Setting more than 20 min may increase power consumption.</span></span>
      <span class="control sleep-after">
        <input type="number" id="sleep-h" min="0" max="23" step="1" inputmode="numeric" aria-label="Hours" ${st.sleepMode === "off" ? "disabled" : ""}> h
        <input type="number" id="sleep-m" min="0" max="59" step="1" inputmode="numeric" aria-label="Minutes" ${st.sleepMode === "off" ? "disabled" : ""}> min
      </span>
    </div>
    <div class="setting ${st.sleepMode === "off" ? "off" : ""}">
      <span class="label">Sleep right after a content update</span>
      <span class="help">${st.sleepMode === "off" ? "Not used while sleep mode is off." : "When content has finished updating, sleep mode is entered immediately."}</span>
      <span class="control"><label class="switch"><input type="checkbox" data-key="sleepAfterUpdate" ${st.sleepAfterUpdate ? "checked" : ""} ${st.sleepAfterUpdate === null || st.sleepAfterUpdate === undefined ? "disabled title='Not reported by this display'" : st.sleepMode === "off" ? "disabled title='Sleep mode is off'" : ""}><span></span></label></span>
    </div>
    <div class="setting">
      <span class="label">Screen protection time</span>
      <span class="help">Refreshes the screen at this time to prevent ghost images, every 3 days according to the app (screen protection measure, mandatory).</span>
      <span class="control">
        <input type="time" data-key="dailyRefresh" value="${esc(st.dailyRefresh ?? "")}">
        <button class="btn small" id="s-refresh-now" title="Refresh the screen now">Now</button>
      </span>
    </div>
    <div class="setting">
      <span class="label">Reset to default settings</span>
      <span class="help">Puts the display's settings back to their defaults and deletes the content stored on it (the app's “Reset All”). Its network settings and display PIN are kept.</span>
      <span class="control"><button class="btn small warn" id="s-reset-settings">Reset…</button></span>
    </div>
    <div class="setting">
      <span class="label">Display PIN</span>
      <span class="help">Asked on every connection, by emdx and the phone app. emdx saves the new one.</span>
      <span class="control"><button class="btn small danger" id="s-change-pin">Change…</button></span>
    </div>`;

  const base = `/api/displays/${enc(d.name)}`;
  // Background colour: a row of swatches, the current one ringed. Clicking one sends it.
  const markBg = (value) => $$("#settings .bg-chip").forEach((c) => {
    const on = c.dataset.bg === value;
    c.classList.toggle("selected", on);
    c.setAttribute("aria-checked", on);
  });
  $$("#settings .bg-chip").forEach((chip) => {
    chip.onclick = async () => {
      const value = chip.dataset.bg;
      if (value === s.settings.backgroundColor) return;
      const prev = s.settings.backgroundColor;
      markBg(value); // optimistic, reverted on failure
      const chips = $$("#settings .bg-chip");
      chips.forEach((c) => (c.disabled = true));
      try {
        await busy(null, () => api("PUT", `${base}/settings`, { backgroundColor: value }));
        s.settings.backgroundColor = value;
        renderStatusParts(); // refresh the swatch and the "About this display" row
      } catch {
        markBg(prev);
      } finally {
        chips.forEach((c) => (c.disabled = false));
      }
    };
  });
  $$("#settings [data-key]").forEach((el) => {
    el.onchange = async () => {
      const value = el.type === "checkbox" ? el.checked : el.value;
      if (value === "") return;
      const key = el.dataset.key;
      if (key === "sleepMode" && value === "deep-sleep" && !(await confirmDialog(
        `Use deep sleep mode on ${d.name}?`,
        "Once it falls asleep, it can't be woken over the network, not even with Wake (Wake-on-LAN): someone has to press its power button.",
        "Use deep sleep",
      ))) { el.value = s.settings[key] ?? ""; return; }
      el.disabled = true;
      try {
        await busy(null, () => api("PUT", `${base}/settings`, { [key]: value }));
        s.settings[key] = value;
        // The display redraws, then posts a new thumbnail by itself
        if (key === "orientation") waitForNewThumb(d.name);
        // Sleep mode and Sleep after set each other: read both back
        if (key === "sleepMode") refreshStatus(d.name);
      } catch {
        if (el.type === "checkbox") el.checked = !el.checked;
        else el.value = s.settings[el.dataset.key] ?? "";
      } finally { el.disabled = false; }
    };
  });
  $("#s-refresh-now").onclick = (e) => busy(e.currentTarget, () => api("POST", `${base}/refresh-now`));
  $("#s-change-pin").onclick = () => openChangePin(d);
  $("#s-reset-settings").onclick = async (e) => {
    const btn = e.currentTarget;
    if (!(await confirmDialog(
      `Reset ${d.name} to default settings?`,
      "Its settings go back to their defaults (on an EM32DX: content player USB, sleep time 20 minutes, screen protection time 01:00) and every image stored on it is deleted: after a while it shows its stock wallpaper. Its network settings and display PIN are kept. This can't be undone.",
      "Reset and delete content",
    ))) return;
    busy(btn, () => api("POST", `${base}/reset`, { kind: "all" }))
      // It restarts and takes a while: keep reading its status until it answers
      .then(() => waitForDisplay(d.name))
      .catch(() => {});
  };

  // Sleep after: hours and minutes. Arrow clicks come in bursts, so only the value
  // left for a second is sent, one request at a time, and the card isn't re-read
  // (the other settings don't depend on the minutes).
  const sh = $("#sleep-h"), sm = $("#sleep-m"), warn = $("#sleep-warn");
  const edit = sleepEdits[d.name] ?? (sleepEdits[d.name] = { sent: null, timer: null, sending: false, again: false });
  edit.sent = edit.timer ? edit.sent : Number.parseInt(st.sleepTime, 10);
  const totalOf = () => (Number.parseInt(sh.value || "0", 10) * 60) + Number.parseInt(sm.value || "0", 10);
  // At 0 h the minutes can't go below 5 (the display's minimum): the arrows stop there
  const setMinutesMin = () => { sm.min = Number.parseInt(sh.value || "0", 10) === 0 ? "5" : "0"; };
  const clampMinutes = () => {
    setMinutesMin();
    const m = Number.parseInt(sm.value, 10);
    if (sm.min === "5" && (!Number.isFinite(m) || m < 5)) sm.value = 5;
  };
  const showSleep = (total) => {
    sh.value = Number.isFinite(total) ? Math.floor(total / 60) : "";
    sm.value = Number.isFinite(total) ? total % 60 : "";
    if (Number.isFinite(total)) clampMinutes();
    warn.hidden = !(total > 20);
  };
  // A status refresh while editing keeps what is being typed
  showSleep(edit.timer || edit.sending ? edit.typed : edit.sent);
  const flush = async () => {
    edit.timer = null;
    // Below 5 at 0 h: send 5, and show it
    if (state.selected === d.name && document.contains(sm)) { clampMinutes(); edit.typed = totalOf(); }
    const total = edit.typed;
    if (!Number.isFinite(total) || total < 5 || total > 23 * 60 + 59) {
      toast("Sleep after must be between 5 min and 23 h 59 min", "error");
      if (state.selected === d.name) showSleep(edit.sent);
      return;
    }
    if (total === edit.sent) return;
    if (edit.sending) { edit.again = true; return; }
    edit.sending = true;
    try {
      await busy(null, () => api("PUT", `${base}/settings`, { sleepTime: String(total) }));
      edit.sent = total;
      if (state.status[d.name]?.settings) state.status[d.name].settings.sleepTime = String(total);
    } catch {
      if (state.selected === d.name) showSleep(edit.sent);
    } finally {
      edit.sending = false;
      if (edit.again) { edit.again = false; flush(); }
    }
  };
  for (const el of [sh, sm]) {
    const changed = (fix) => () => {
      // While typing ("1" on the way to "12") only the arrows' limit moves; leaving
      // the field (or the send) brings a too-small value up to 5
      if (fix) clampMinutes(); else setMinutesMin();
      edit.typed = totalOf();
      warn.hidden = !(edit.typed > 20);
      clearTimeout(edit.timer);
      edit.timer = setTimeout(flush, 1000);
    };
    el.oninput = changed(false);
    el.onchange = changed(true);
  }

  const rows = [];
  const add = (k, v) => { if (v !== null && v !== undefined && v !== "") rows.push([k, v]); };
  add("Name on the display", s.deviceName);
  const info = Object.fromEntries((s.info || []).map((i) => [i.label, i.value]));
  add("Model", info["Model"]);
  add("Serial number", s.serial);
  add("Software", s.software);
  add("Sub-micom", info["Sub-micom"]);
  add("Wi-Fi MAC", s.wifiMac);
  add("Bluetooth MAC", info["Bluetooth MAC"]);
  add("Wired MAC", info["Wired MAC"]);
  if (st.orientation) add("Orientation", st.orientation === "portrait" ? "Portrait" : "Landscape");
  if (st.backgroundColor) add("Background colour", st.backgroundColor);
  add("Clock", s.clock);
  add("Setup", s.setupDone === null || s.setupDone === undefined ? null : s.setupDone ? "completed" : "not completed");
  add("Terms", s.termsAccepted === true ? "accepted" : s.termsAccepted === false ? "not accepted" : null);
  if (s.features) add("Low-power Wi-Fi", s.features.lowPowerWifi ? "supported" : "no");
  add("Unique ID", info["Unique ID"]);
  add("Unique device ID", info["Unique device ID"]);
  $("#about").innerHTML = `<dl class="kv">${rows.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join("")}</dl>`;
  // The status read gave the server this display's serial, so its thumbnail can be found now
  refreshThumb(d.name);
}

// ─── Thumbnail of what the display is showing ────────────────────────────────

/** name -> { old: saved path of the outdated thumbnail, timer } */
const thumbWaits = {};
/** name -> saved path of the thumbnail last shown */
const thumbShown = {};
const THUMB_WAIT_MS = 120000;

/** name -> { phase: sending | sent | shown | failed | stale, detail, at } of the last image sent */
const uploads = {};

function setUpload(name, phase, detail = "") {
  uploads[name] = { phase, detail, at: new Date() };
  renderUpload(name);
}

/** The line under "Show an image": where the last image is on its way to the screen */
function renderUpload(name) {
  const el = $("#upload-status");
  if (!el || state.selected !== name) return;
  const u = uploads[name];
  // The drop zone's outline: grey at rest, orange while sending, green once sent, red on failure
  const drop = $("#drop");
  if (drop) {
    drop.classList.remove("sending", "sent", "failed");
    if (u) drop.classList.add({ sending: "sending", sent: "sent", shown: "sent", stale: "sent", failed: "failed" }[u.phase]);
  }
  el.hidden = !u;
  if (!u) return;
  const time = u.at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  const text = {
    sending: "Sending…<br>The display is downloading the image.",
    sent: `Sent at ${time}.<br>Waiting for the display to redraw (e-paper takes a while)…`,
    shown: `✓ On the screen since ${time}.`,
    stale: "Sent, but the display hasn't reported its new screen yet.",
    failed: `Couldn't send it: ${esc(u.detail)}`,
  }[u.phase];
  el.className = `upload-status ${u.phase}`;
  el.innerHTML = text;
}

/**
 * name -> the slideshow of that display, as saved on the server (`slideshow/saved`)
 * plus what is being edited: items are `{ id, url }` once saved, `{ file, url }`
 * while new. Saved a moment after each change.
 */
const slideshows = {};

function slideshowOf(name) {
  return slideshows[name] ?? (slideshows[name] = {
    loaded: false, items: [], interval: 60, title: "", status: null,
    timer: null, saving: null, again: false, save: null, error: null,
  });
}

/** 90 -> "1 h 30 min" */
function everyText(minutes) {
  const h = Math.floor(minutes / 60), m = minutes % 60;
  return h ? `${h} h${m ? ` ${m} min` : ""}` : `${m} min`;
}

/** Read the saved slideshow once per page load */
async function loadSlideshow(name) {
  const show = slideshowOf(name);
  if (show.loaded) return show;
  const saved = await api("GET", `/api/displays/${enc(name)}/slideshow/saved`);
  show.items = saved.images.map((i) => ({ id: i.id, url: i.url }));
  show.interval = saved.interval;
  show.title = saved.name;
  show.loaded = true;
  return show;
}

/** Save now; while a save runs, one more follows with the latest state */
async function saveSlideshow(name) {
  const show = slideshowOf(name);
  clearTimeout(show.timer);
  show.timer = null;
  if (show.saving) { show.again = true; return show.saving; }
  const fd = new FormData();
  fd.append("name", show.title.trim());
  fd.append("interval", String(show.interval));
  for (const it of show.items) {
    if (it.id) fd.append("slide", `id:${it.id}`);
    else fd.append("slide", it.file, it.file.name);
  }
  const sent = show.items.slice();
  show.save = "saving";
  renderSaveState(name);
  show.saving = (async () => {
    try {
      const saved = await api("PUT", `/api/displays/${enc(name)}/slideshow/saved`, fd);
      // New images now have an id; keep any change made while saving
      sent.forEach((it, i) => {
        if (it.id) return;
        it.id = saved.images[i].id;
        URL.revokeObjectURL(it.url);
        it.url = saved.images[i].url;
        delete it.file;
      });
      show.save = "saved";
      show.error = null;
    } catch (e) {
      show.save = "failed";
      show.error = e.message;
    }
  })();
  await show.saving;
  show.saving = null;
  renderSaveState(name);
  if (show.again) { show.again = false; return saveSlideshow(name); }
}

/** Save a moment after the last change (typing, dragging...) */
function saveSoon(name) {
  const show = slideshowOf(name);
  clearTimeout(show.timer);
  show.save = "pending";
  renderSaveState(name);
  show.timer = setTimeout(() => saveSlideshow(name), 700);
}

function renderSaveState(name) {
  const el = $("#slideshow-saved");
  if (!el || state.selected !== name) return;
  const show = slideshowOf(name);
  el.className = `muted small save-state ${show.save ?? ""}`;
  el.textContent = {
    pending: "Unsaved changes…",
    saving: "Saving…",
    saved: "Saved",
    failed: `Couldn't save: ${show.error}`,
  }[show.save] ?? "";
}

/** The Slideshow card: what is saved, Edit and Send */
function wireSlideshow(d) {
  const show = slideshowOf(d.name);
  const summary = $("#slide-summary"), send = $("#slide-send");
  const render = () => {
    const n = show.items.length;
    summary.innerHTML = !show.loaded
      ? '<p class="muted small">Loading…</p>'
      : n
        ? `<div class="slide-strip">${show.items.slice(0, 10).map((it) => `<img alt="" src="${esc(it.url)}">`).join("")}${n > 10 ? `<span class="muted small">+${n - 10}</span>` : ""}</div>
           <p class="muted small">${show.title ? `${esc(show.title)} · ` : ""}${n} image${n === 1 ? "" : "s"}, each shown for ${everyText(show.interval)}${n > 1 ? `; one full loop takes ${everyText(n * show.interval)}` : ""}.</p>`
        : '<p class="muted small">No slideshow yet: open the editor to add images.</p>';
    send.classList.toggle("inactive", !n);
  };
  $("#slide-edit").onclick = async () => {
    try { await loadSlideshow(d.name); } catch (e) { toast(e.message, "error"); return; }
    openSlideshowEditor(d, render);
  };
  send.onclick = () => sendSlideshow(d);
  render();
  renderSlideStatus(d.name);
  loadSlideshow(d.name).then(() => { if (state.selected === d.name) render(); }).catch(() => {});
}

/** The status line, on the card and in the editor */
function renderSlideStatus(name) {
  if (state.selected !== name) return;
  const st = slideshowOf(name).status;
  for (const el of [$("#slide-status"), $("#slideshow-status")]) {
    if (!el) continue;
    el.hidden = !st;
    if (!st) continue;
    el.className = `upload-status ${st.kind}`;
    el.innerHTML = st.html;
  }
}

/** Send the saved slideshow (saving the last changes first) */
async function sendSlideshow(d) {
  const show = slideshowOf(d.name);
  const buttons = [$("#slide-send"), $("#slideshow-send")].filter(Boolean);
  if (!show.items.length) {
    show.status = { kind: "failed", html: "Add an image first." };
    renderSlideStatus(d.name);
    return;
  }
  buttons.forEach((b) => { b.disabled = true; b.classList.add("busy"); });
  try {
    await saveSlideshow(d.name);
    if (show.save === "failed") throw new Error(`the slideshow couldn't be saved (${show.error})`);
    const n = show.items.length, every = everyText(show.interval);
    show.status = { kind: "sending", html: `Sending ${n} image${n === 1 ? "" : "s"}…<br>The display downloads them one by one.` };
    renderSlideStatus(d.name);
    waitForNewThumb(d.name, "Sending the slideshow; the display posts its thumbnail once it has redrawn…");
    await api("POST", `/api/displays/${enc(d.name)}/slideshow/send`);
    const time = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    show.status = { kind: "shown", html: `Sent at ${time}: ${n} image${n === 1 ? "" : "s"}, each shown for ${every}.<br>The display now runs it on its own.` };
    // In the editor, swap the form for a confirmation; sent from the card instead,
    // flash the card green. (The card keeps its "Sent at…" status either way.)
    if ($("#dlg-slideshow")?.open) showSlideshowSent(d.name, n, every, time);
    else flashCard("#card-slideshow");
    if (state.status[d.name]?.settings?.contentPlayer !== "mobile") refreshStatus(d.name);
  } catch (e) {
    show.status = { kind: "failed", html: `Couldn't send it: ${esc(e.message)}` };
    cancelThumbWait(d.name);
  } finally {
    buttons.forEach((b) => { b.disabled = false; b.classList.remove("busy"); });
  }
  renderSlideStatus(d.name);
}

/**
 * The slideshow editor: name, images (drag to reorder, drop to add) and the time
 * between images. Every change is saved; `onChange` updates the card.
 */
function openSlideshowEditor(d, onChange) {
  const show = slideshowOf(d.name);
  const dlg = $("#dlg-slideshow");
  const grid = $("#slideshow-grid"), title = $("#slideshow-name");
  const sh = $("#slideshow-h"), sm = $("#slideshow-m"), loop = $("#slideshow-loop");
  const changed = () => { saveSoon(d.name); onChange(); };
  // Tiles in the display's shape: images keep their proportions on the panel
  grid.classList.toggle("portrait", state.status[d.name]?.settings?.orientation === "portrait");
  $("#slideshow-title").textContent = `Slideshow for ${d.name}`;
  title.value = show.title;
  title.oninput = () => { show.title = title.value; changed(); };

  let dragFrom = null;
  const move = (from, to) => {
    if (from === to || from < 0 || to < 0 || to >= show.items.length) return;
    const [it] = show.items.splice(from, 1);
    show.items.splice(to, 0, it);
  };
  const addFiles = (files) => {
    let added = 0;
    for (const f of files) {
      if (!f.type.startsWith("image/")) continue;
      show.items.push({ file: f, url: URL.createObjectURL(f) });
      added++;
    }
    if (added) { render(); changed(); }
  };
  const render = () => {
    const n = show.items.length;
    grid.innerHTML = show.items.map((it, i) => `
      <figure class="slide-tile" draggable="true" data-i="${i}" title="${esc(it.file?.name ?? `Image ${i + 1}`)}">
        <span class="slide-n">${i + 1}</span>
        <img alt="" src="${esc(it.url)}" draggable="false">
        <figcaption>
          <button type="button" class="btn small ghost" data-act="left" ${i === 0 ? "disabled" : ""} title="Earlier" aria-label="Move earlier">←</button>
          <button type="button" class="btn small ghost" data-act="right" ${i === n - 1 ? "disabled" : ""} title="Later" aria-label="Move later">→</button>
          <button type="button" class="btn small ghost" data-act="remove" title="Remove" aria-label="Remove">✕</button>
        </figcaption>
      </figure>`).join("") + `
      <label class="slide-tile slide-add" title="Add images">
        <span>＋</span><span class="small">Add images</span>
        <input type="file" accept="image/*" multiple hidden>
      </label>`;
    $(".slide-add input", grid).onchange = (e) => { addFiles(e.target.files); e.target.value = ""; };
    $$("figure.slide-tile", grid).forEach((tile) => {
      const i = Number(tile.dataset.i);
      tile.querySelectorAll("button").forEach((b) => (b.onclick = () => {
        if (b.dataset.act === "left") move(i, i - 1);
        else if (b.dataset.act === "right") move(i, i + 1);
        else {
          const [it] = show.items.splice(i, 1);
          if (it.file) URL.revokeObjectURL(it.url);
        }
        render();
        changed();
      }));
      tile.ondragstart = (e) => { dragFrom = i; tile.classList.add("dragging"); e.dataTransfer.effectAllowed = "move"; };
      tile.ondragend = () => { dragFrom = null; tile.classList.remove("dragging"); };
      tile.ondragover = (e) => {
        if (dragFrom === null) return;
        e.preventDefault();
        const r = tile.getBoundingClientRect(), after = e.clientX > r.left + r.width / 2;
        tile.classList.toggle("drop-before", !after);
        tile.classList.toggle("drop-after", after);
      };
      tile.ondragleave = () => tile.classList.remove("drop-before", "drop-after");
      tile.ondrop = (e) => {
        if (dragFrom === null) return;
        e.preventDefault();
        const after = tile.classList.contains("drop-after");
        tile.classList.remove("drop-before", "drop-after");
        let to = after ? i + 1 : i;
        if (dragFrom < to) to -= 1;
        move(dragFrom, to);
        dragFrom = null;
        render();
        changed();
      };
    });
    loop.textContent = n
      ? `${n} image${n === 1 ? "" : "s"}${n > 1 ? ` · full loop ${everyText(n * show.interval)}` : ""}`
      : "None yet";
    $("#slideshow-send").classList.toggle("inactive", !n);
  };
  // Files dropped from the desktop add images (tiles being dragged reorder instead)
  grid.ondragover = (e) => { if (dragFrom === null && e.dataTransfer.types.includes("Files")) { e.preventDefault(); grid.classList.add("over"); } };
  grid.ondragleave = (e) => { if (e.target === grid) grid.classList.remove("over"); };
  grid.ondrop = (e) => {
    grid.classList.remove("over");
    if (dragFrom !== null || !e.dataTransfer.files.length) return;
    e.preventDefault();
    addFiles(e.dataTransfer.files);
  };

  // Time between images: 5 min to 24 h
  const showInterval = () => { sh.value = Math.floor(show.interval / 60); sm.value = show.interval % 60; sm.min = sh.value === "0" ? "5" : "0"; };
  sh.oninput = sm.oninput = () => { sm.min = Number.parseInt(sh.value || "0", 10) === 0 ? "5" : "0"; };
  sh.onchange = sm.onchange = () => {
    let t = Number.parseInt(sh.value || "0", 10) * 60 + Number.parseInt(sm.value || "0", 10);
    if (!Number.isFinite(t) || t < 5) t = 5;
    if (t > 24 * 60) t = 24 * 60;
    show.interval = t;
    showInterval();
    render();
    changed();
  };

  $("#slideshow-send").onclick = () => sendSlideshow(d);
  // Closing saves what is still pending
  dlg.onclose = () => { if (show.timer) saveSlideshow(d.name); onChange(); };
  // Always open on the editor, not a leftover "sent" confirmation
  dlg.querySelector("form").hidden = false;
  $("#slideshow-sent").hidden = true;
  showInterval();
  render();
  renderSaveState(d.name);
  renderSlideStatus(d.name);
  dlg.showModal();
}

/** name -> { entries: [{ at: "HH:MM", file, url }], status } of the schedule being prepared */
const schedules = {};

/** The Schedule card: start times, each with the image shown from then on */
function wireSchedule(d) {
  const blank = (at) => ({ at, file: null, url: null });
  const sched = schedules[d.name] ?? (schedules[d.name] = { entries: [blank("08:00")], status: null });
  const list = $("#sched"), add = $("#sched-add"), send = $("#sched-send"), status = $("#sched-status");
  /** Why it can't be sent yet, or null */
  const problem = () => {
    const seen = new Set();
    for (const e of sched.entries) {
      if (!e.at) return "Set a time for every entry.";
      if (seen.has(e.at)) return `Two entries start at ${e.at}: each time can be used once.`;
      seen.add(e.at);
      if (!e.file) return `Choose an image for ${e.at}.`;
    }
    return sched.entries.length ? null : "Add a time first.";
  };
  const toggle = () => {
    const why = problem();
    send.classList.toggle("inactive", !!why);
    // A "fix this first" message goes away, or follows, as the entries change
    if (sched.status?.check) {
      sched.status = why ? { kind: "failed", check: true, html: esc(why) } : null;
      renderStatus();
    }
  };
  const render = () => {
    list.innerHTML = sched.entries.map((e, i) => `<div class="sched-row" data-i="${i}">
        ${e.file ? `<img alt="" src="${esc(e.url)}">` : '<span class="sched-none muted small">No image</span>'}
        <label class="sched-at">From <input type="time" value="${esc(e.at)}" data-f="at" aria-label="Start time"></label>
        <span class="grow"></span>
        <label class="btn small ghost">${e.file ? "Change image" : "Choose image"}<input type="file" accept="image/*" data-f="file" hidden></label>
        <button type="button" class="btn small ghost" data-act="drop" title="Remove this time" ${sched.entries.length === 1 ? "disabled" : ""}>Remove</button>
      </div>`).join("");
    $$(".sched-row", list).forEach((row) => {
      const i = Number(row.dataset.i), e = sched.entries[i];
      const at = row.querySelector('[data-f="at"]'), file = row.querySelector('[data-f="file"]');
      at.onchange = () => { e.at = at.value; toggle(); };
      file.onchange = () => {
        const f = file.files[0];
        if (!f || !f.type.startsWith("image/")) return;
        if (e.url) URL.revokeObjectURL(e.url);
        e.file = f;
        e.url = URL.createObjectURL(f);
        render();
      };
      row.querySelector('[data-act="drop"]').onclick = () => {
        if (e.url) URL.revokeObjectURL(e.url);
        sched.entries.splice(i, 1);
        render();
      };
    });
    toggle();
  };
  add.onclick = () => {
    // An hour after the latest entry, as a starting point
    const last = sched.entries.map((e) => e.at).filter(Boolean).sort().pop();
    const [h, m] = (last ?? "07:00").split(":").map(Number);
    sched.entries.push(blank(`${String((h + 1) % 24).padStart(2, "0")}:${String(m).padStart(2, "0")}`));
    render();
  };
  const renderStatus = () => {
    status.hidden = !sched.status;
    if (!sched.status) return;
    status.className = `upload-status ${sched.status.kind}`;
    status.innerHTML = sched.status.html;
  };
  send.onclick = async () => {
    const why = problem();
    if (why) {
      sched.status = { kind: "failed", check: true, html: esc(why) };
      renderStatus();
      return;
    }
    const entries = [...sched.entries].sort((a, b) => a.at.localeCompare(b.at));
    const fd = new FormData();
    entries.forEach((e) => { fd.append("at", e.at); fd.append("image", e.file, e.file.name); });
    const times = entries.map((e) => esc(e.at)).join(", ");
    sched.status = { kind: "sending", html: "Sending the schedule…<br>The display downloads the images one by one." };
    renderStatus();
    waitForNewThumb(d.name, "Sending the schedule; the display posts its thumbnail once it has redrawn…");
    try {
      await busy(send, () => api("POST", `/api/displays/${enc(d.name)}/schedule`, fd), { quiet: true });
      const time = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
      sched.status = { kind: "shown", html: `Sent at ${time}. The image changes every day at ${times}.<br>The display now runs it on its own.` };
      if (state.status[d.name]?.settings?.contentPlayer !== "mobile") refreshStatus(d.name);
    } catch (e) {
      sched.status = { kind: "failed", html: `Couldn't send it: ${esc(e.message)}` };
      cancelThumbWait(d.name);
    }
    if (state.selected === d.name) renderStatus();
  };
  render();
  renderStatus();
}

/** name -> image URL of the thumbnail last shown */
const thumbShownImage = {};

/**
 * The thumbnail on screen no longer matches the display (new image sent, or a new
 * orientation): darken it under a spinner until the display posts a new one.
 * `message` says what is happening.
 */
function waitForNewThumb(name, message = "Waiting for the display to redraw and send a new thumbnail…") {
  clearTimeout(thumbWaits[name]?.timer);
  thumbWaits[name] = {
    old: thumbShown[name] ?? null,
    message,
    timer: setTimeout(() => {
      delete thumbWaits[name];
      if (uploads[name]?.phase === "sent") setUpload(name, "stale");
      if (state.selected !== name) return;
      refreshThumb(name).then(() => {
        const note = $("#thumb p");
        if (note) note.textContent = "The display hasn't sent a new thumbnail yet: this one may be outdated.";
      });
    }, THUMB_WAIT_MS),
  };
  if (state.selected === name) renderThumbWaiting(name);
}

/** Stop waiting (the image wasn't sent after all) and show the last thumbnail again */
function cancelThumbWait(name) {
  const wait = thumbWaits[name];
  if (!wait) return;
  clearTimeout(wait.timer);
  delete thumbWaits[name];
  if (state.selected === name) refreshThumb(name);
}

function renderThumbWaiting(name) {
  const box = $("#thumb");
  if (!box) return;
  const message = esc(thumbWaits[name]?.message ?? "Waiting for a new thumbnail…");
  const image = thumbShownImage[name];
  box.innerHTML = image
    ? `<div class="thumb-replacing"><img alt="" src="${esc(image)}"><span class="spinner"></span></div>
       <p class="muted small">${message}</p>`
    : `<div class="thumb-wait"><span class="spinner"></span><p class="muted small">${message}</p></div>`;
}

function renderThumb(d, t) {
  const box = $("#thumb");
  if (!box) return;
  const wait = thumbWaits[d.name];
  if (wait) {
    // Still the outdated one: keep waiting
    if (!t || t.saved === wait.old) { renderThumbWaiting(d.name); return; }
    clearTimeout(wait.timer);
    delete thumbWaits[d.name];
    // The new image is up: the display redrew and reported it
    if (uploads[d.name] && ["sending", "sent"].includes(uploads[d.name].phase)) setUpload(d.name, "shown");
  }
  if (t) { thumbShown[d.name] = t.saved; thumbShownImage[d.name] = t.image; }
  if (!t) {
    box.innerHTML = '<p class="muted small">No thumbnail yet.</p>';
    return;
  }
  const meta = [t.contentName, t.contentType, t.scheduleDownloadedTimestamp]
    .filter(Boolean).map(esc).join(" · ");
  box.innerHTML = `<img alt="What ${esc(d.name)} is showing" src="${esc(t.image)}"
      title="Show it at full size" tabindex="0" role="button">
    <p class="muted small">${meta || "uploaded by the display"}</p>`;
  const img = $("img", box);
  img.onclick = () => openLightbox(img);
  img.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); openLightbox(img); } };
}

/** The thumbnail at its own size; a click anywhere (or Escape) closes it. */
function openLightbox(img) {
  const dlg = $("#dlg-thumb"), big = $("img", dlg);
  big.src = img.src;
  big.alt = img.alt;
  dlg.onclick = () => dlg.close();
  dlg.showModal();
}

const thumbAsking = new Set();

/**
 * Ask the display for a fresh thumbnail. `quiet` (when a display is opened) keeps
 * failures out of the toasts: the card just keeps the last one it had.
 */
async function askThumb(name, { quiet = false } = {}) {
  if (thumbAsking.has(name)) return;
  thumbAsking.add(name);
  const btn = () => (state.selected === name ? $("#t-ask") : null);
  const setBusy = (on) => { const b = btn(); if (b) { b.disabled = on; b.classList.toggle("busy", on); } };
  setBusy(true);
  try {
    const r = await api("POST", `/api/displays/${enc(name)}/thumbnail`);
    if (state.selected === name) renderThumb(display(name), r.thumbnail);
    if (!quiet) toast(r.message);
  } catch (e) {
    if (!quiet) toast(e.message, "error");
  } finally {
    thumbAsking.delete(name);
    setBusy(false);
  }
}

/** Show whatever this display has uploaded, if it ever did. */
async function refreshThumb(name) {
  if (!$("#thumb") || state.selected !== name) return;
  try {
    renderThumb(display(name), await api("GET", `/api/displays/${enc(name)}/thumbnail`));
  } catch {
    // Nothing on record yet, which is what the card already says
  }
}

// ─── EMDX Settings ───────────────────────────────────────────────────────────

/** Name, IP, PIN and MAC that emdx (CLI and web) uses to reach the display. */
function openNetwork(d) {
  const dlg = $("#dlg-network"), form = $("#form-network");
  const base = `/api/displays/${enc(d.name)}`;
  form.reset();
  $(".form-error", form).hidden = true;
  form.elements.rename.value = d.name;
  form.elements.host.value = d.host;
  form.elements.pin.placeholder = `${d.pin} (unchanged)`;
  form.elements.mac.value = d.mac ?? "";
  // Pairing mode or erase, then removed from emdx (its own dialog)
  $("#network-forget").onclick = () => { dlg.close(); openForget(d); };
  const def = $("#network-default");
  def.hidden = !!d.default;
  def.onclick = (e) =>
    busy(e.currentTarget, () => api("POST", `${base}/default`))
      .then(() => { dlg.close(); return loadDisplays(d.name); })
      .catch(() => {});
  form.onsubmit = async (e) => {
    e.preventDefault();
    const f = new FormData(form);
    const body = { rename: f.get("rename").trim(), host: f.get("host").trim(), mac: f.get("mac").trim() };
    if (f.get("pin")) body.pin = f.get("pin");
    const err = $(".form-error", form);
    err.hidden = true;
    try {
      const r = await busy($("#network-save"), () => api("PATCH", base, body), { quiet: true });
      dlg.close();
      toast("Saved");
      await loadDisplays(r.name);
    } catch (e) {
      err.textContent = e.message;
      err.hidden = false;
    }
  };
  dlg.showModal();
}

/** After a reset: read the status every 15 s (up to 3 min) until the display answers. */
async function waitForDisplay(name) {
  const until = Date.now() + 180000;
  await new Promise((r) => setTimeout(r, 15000));
  while (Date.now() < until) {
    await refreshStatus(name);
    if (!state.status[name]?.error) return;
    await new Promise((r) => setTimeout(r, 15000));
  }
}

// ─── Change PIN ──────────────────────────────────────────────────────────────

/** Change the PIN on the display itself (the app's "Change PIN"), then save it. */
function openChangePin(d) {
  const dlg = $("#dlg-pin"), form = $("#form-pin"), err = $(".form-error", form);
  form.reset();
  err.hidden = true;
  $("#pin-title").textContent = `Change ${d.name}'s PIN`;
  // Digits only, at most 6; the button waits for two identical 6-digit PINs
  const save = $("#pin-save"), check = $("#pin-check");
  const { pin: first, again: second } = form.elements;
  const sync = () => {
    for (const el of [first, second]) {
      const digits = el.value.replace(/\D/g, "").slice(0, 6);
      if (el.value !== digits) el.value = digits;
    }
    const ok6 = (v) => v.length === 6;
    const match = ok6(first.value) && first.value === second.value;
    save.disabled = !match;
    check.textContent = !first.value ? ""
      : !ok6(first.value) ? `${6 - first.value.length} more digit${first.value.length === 5 ? "" : "s"}`
      : !second.value ? "Enter the PIN again."
      : !ok6(second.value) ? `${6 - second.value.length} more digit${second.value.length === 5 ? "" : "s"} in the second field`
      : match ? "The PINs match." : "The PINs you entered do not match.";
  };
  first.oninput = second.oninput = sync;
  sync();
  form.onsubmit = async (e) => {
    e.preventDefault();
    err.hidden = true;
    const pin = form.elements.pin.value.trim(), again = form.elements.again.value.trim();
    const fail = (msg) => { err.textContent = msg; err.hidden = false; };
    if (!/^\d{6}$/.test(pin)) return fail("Your PIN must contain 6 numbers.");
    if (pin !== again) return fail("The PINs you entered do not match. Please try again.");
    try {
      const r = await busy($("#pin-save"), () => api("POST", `/api/displays/${enc(d.name)}/change-pin`, { newPin: pin }));
      dlg.close();
      await loadDisplays(d.name);
      return r;
    } catch (e) {
      fail(e.message);
    }
  };
  dlg.showModal();
}

// ─── Forget ──────────────────────────────────────────────────────────────────

function openForget(d) {
  const dlg = $("#dlg-forget"), form = $("#form-forget");
  form.reset();
  $("#forget-title").textContent = `Forget ${d.name}?`;
  $(".form-error", form).hidden = true;
  $("#forget-anyway").hidden = true;

  const done = async (msg) => {
    toast(msg);
    dlg.close();
    delete state.status[d.name];
    state.selected = null;
    await loadDisplays();
  };
  form.onsubmit = async (e) => {
    e.preventDefault();
    const err = $(".form-error", form);
    err.hidden = true;
    try {
      const r = await busy($("#forget-go"), () => api("POST", `/api/displays/${enc(d.name)}/forget`), { quiet: true });
      await done(r.message);
    } catch (ex) {
      err.textContent = `Couldn't reach the display, so it wasn't changed or removed: ${ex.message}`;
      err.hidden = false;
      $("#forget-anyway").hidden = false;
    }
  };
  $("#forget-anyway").onclick = async (e) => {
    try {
      await busy(e.currentTarget, () => api("DELETE", `/api/displays/${enc(d.name)}`), { quiet: true });
      await done(`"${d.name}" was removed from emdx (the display itself was not changed).`);
    } catch { /* toasted */ }
  };
  dlg.showModal();
}

// ─── Add / discover ──────────────────────────────────────────────────────────

function openAdd(prefill = {}) {
  const dlg = $("#dlg-add"), form = $("#form-add");
  form.reset();
  for (const [k, v] of Object.entries(prefill)) if (form.elements[k]) form.elements[k].value = v ?? "";
  $(".form-error", form).hidden = true;
  dlg.showModal();
  searchNetwork();
}

const slug = (name) => (name || "display").toLowerCase().replace(/[^a-z0-9_-]+/g, "-").replace(/^-|-$/g, "").slice(0, 32);

async function searchNetwork() {
  const btn = $("#btn-discover"), status = $("#discover-status"), list = $("#discover-list");
  const form = $("#form-add");
  list.innerHTML = "";
  status.innerHTML = '<span class="dot wait"></span> Searching the local network…';
  try {
    const { displays: found, moved } = await busy(btn, () => api("GET", "/api/discover"), { quiet: true });
    status.textContent = found.length
      ? "Pick one to fill in its address, then enter its PIN."
      : "No display answered. Sleeping displays don't respond: wake them, or enter the address below.";
    if (moved.length) {
      // Saved displays that changed address: the list and their cards follow
      toast(moved.map((m) => `${m.name} is now at ${m.to} (was ${m.from})`).join(" · "));
      await loadDisplays();
    }
    list.innerHTML = found.map((d, i) => `<li data-i="${i}" class="${d.saved ? "disabled" : ""}">
        <span><strong>${esc(d.name || d.model)}</strong><br><span class="mono muted">${esc(d.host)}${d.mac ? " · " + esc(d.mac) : ""}</span></span>
        ${d.saved ? `<span class="badge">saved as ${esc(d.saved)}</span>` : '<span class="muted small">select</span>'}
      </li>`).join("");
    $$("li", list).forEach((li) => {
      const d = found[li.dataset.i];
      if (d.saved) return;
      li.onclick = () => {
        $$("li", list).forEach((x) => x.classList.toggle("selected", x === li));
        form.elements.host.value = d.host;
        form.elements.mac.value = d.mac || "";
        if (!form.elements.name.value) form.elements.name.value = slug(d.name);
        form.elements.pin.focus();
      };
    });
  } catch (e) { status.textContent = e.message; }
}
$("#btn-discover").onclick = searchNetwork;

$("#form-add").addEventListener("submit", async (e) => {
  const form = e.target;
  if (e.submitter?.value !== "ok") return;
  e.preventDefault();
  const f = new FormData(form);
  const body = {
    name: f.get("name").trim(), host: f.get("host").trim(), pin: f.get("pin").trim(),
    mac: f.get("mac").trim() || null, verify: !!f.get("verify"), make_default: !!f.get("make_default"),
  };
  const err = $(".form-error", form);
  err.hidden = true;
  try {
    await busy(e.submitter, () => api("POST", "/api/displays", body));
    $("#dlg-add").close();
    loadDisplays(body.name);
  } catch (ex) { err.textContent = ex.message; err.hidden = false; }
});

// ─── Bluetooth setup ─────────────────────────────────────────────────────────

let provisionAddress = null;

function openProvision() {
  const form = $("#form-provision");
  form.reset();
  provisionAddress = null;
  $("#scan-list").innerHTML = "";
  $("#scan-status").textContent = "";
  $("#provision-log").hidden = true;
  $("#provision-log").textContent = "";
  $(".form-error", form).hidden = true;
  $("#dlg-provision").showModal();
  syncProvisionButton();
  scan();
  scanWifi();
}

/** Signal 0–100 as four bars, in text (a dropdown option can't hold markup) */
function signalText(signal) {
  const lit = signal >= 75 ? 4 : signal >= 50 ? 3 : signal >= 25 ? 2 : 1;
  return `${"▂▄▆█".slice(0, lit)}${"·".repeat(4 - lit)} ${signal}%`;
}

/**
 * The Wi-Fi networks around this computer (2.4 GHz only), in a dropdown. The scan
 * takes a few seconds; "Other network…" (or no list at all, when this computer
 * may not scan) shows the free name field.
 */
async function scanWifi(force = false) {
  const btn = $("#btn-wifi-scan"), status = $("#wifi-status"), select = $("#wifi-select");
  const pick = $("#wifi-pick"), ssidField = $("#ssid-field");
  const scanning = () => {
    pick.hidden = false;
    ssidField.hidden = true;
    select.disabled = true;
    select.innerHTML = '<option value="">Scanning for Wi-Fi networks…</option>';
    status.className = "wifi-status muted small scanning";
    status.innerHTML = '<span class="spinner"></span> Scanning for Wi-Fi networks… this takes a few seconds.';
  };
  let found;
  try {
    // The background scan's list, at once; a scan only when asked or none yet
    found = force ? null : await api("GET", "/api/wifi");
    if (force || found.available === null) {
      scanning();
      found = await busy(btn, () => api("POST", "/api/wifi/scan"), { quiet: true });
    }
  } catch (e) {
    found = { available: false, reason: e.message };
  }
  renderWifi(found);
}

/** The dropdown from a `/api/wifi` answer; keeps what was picked or typed */
function renderWifi(found) {
  const status = $("#wifi-status"), select = $("#wifi-select");
  const pick = $("#wifi-pick"), ssidField = $("#ssid-field"), hint = $("#wifi-hint");
  const form = $("#form-provision"), { ssid, password } = form.elements;
  // Networks are numbered from 0, so this can't clash with one
  const OTHER = "other";
  const showName = (on) => { ssidField.hidden = !on; };
  status.className = "wifi-status muted small";
  if (!found.available) {
    // This computer can't scan (e.g. emdx isn't allowed to): just the name field
    pick.hidden = true;
    pick.title = found.reason || "";
    showName(true);
    hint.textContent = "The display only joins 2.4 GHz networks. Its address is assigned automatically (DHCP).";
    return;
  }
  hint.textContent = "The display only joins 2.4 GHz networks: only those are listed (as seen from this computer). Its address is assigned automatically (DHCP).";
  pick.hidden = false;

  const nets = found.networks;
  const typed = ssid.value;
  const otherChosen = select.value === OTHER;
  select.innerHTML = [
    `<option value="">${nets.length ? "Choose a network…" : "No 2.4 GHz network found"}</option>`,
    ...nets.map((n, i) => `<option value="${i}" ${n.enterprise ? "disabled" : ""}>${esc(
      `${n.ssid}  —  ${signalText(n.signal)}  ·  ${n.enterprise ? "enterprise, not supported" : n.security || "open"}`,
    )}</option>`),
    `<option value="${OTHER}">Other network (hidden or not listed)…</option>`,
  ].join("");
  select.disabled = false;
  const at = found.scannedAt ? new Date(found.scannedAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : null;
  status.textContent = `${nets.length} network${nets.length === 1 ? "" : "s"} on 2.4 GHz${at ? `, scanned at ${at}` : ""}.`;

  select.onchange = () => {
    const v = select.value;
    if (v === OTHER) {
      showName(true);
      ssid.value = "";
      ssid.focus();
      return;
    }
    showName(false);
    const n = nets[v];
    ssid.value = n ? n.ssid : "";
    password.value = "";
    password.placeholder = n && !n.security ? "none: open network" : "";
    if (n && n.security) password.focus();
    syncProvisionButton();
  };
  // Keep what was already chosen or typed across an update
  const again = nets.findIndex((n) => n.ssid === typed);
  if (typed && again >= 0 && !nets[again].enterprise && !otherChosen) {
    select.value = String(again);
    ssidField.hidden = true;
  } else if (typed || otherChosen) {
    select.value = OTHER;
    showName(true);
  } else {
    showName(false);
  }
  syncProvisionButton();
}


async function scan() {
  const btn = $("#btn-scan"), status = $("#scan-status"), list = $("#scan-list");
  status.textContent = "Scanning for 8 seconds…";
  try {
    const found = await busy(btn, () => api("GET", "/api/ble/scan"), { quiet: true });
    const setup = found.filter((f) => f.setupMode);
    status.textContent = setup.length
      ? `${setup.length} display${setup.length > 1 ? "s" : ""} in setup mode`
      : `${found.length ? "Displays seen, but none in pairing mode." : "No display seen."} Press the Bluetooth button on the back of the screen: the LED blinks blue. If nothing happens, press its power button first (the LED turns green while it wakes up), then the Bluetooth button, and scan again.`;
    list.innerHTML = found.map((f, i) => `<li data-i="${i}" class="${f.setupMode ? "" : "disabled"}">
        <span><strong>${esc(f.name || "Unnamed")}</strong><br><span class="mono muted">${esc(f.address)}</span></span>
        <span class="muted small">${f.setupMode ? (f.setUpBefore ? "pairing mode · set up before: its display PIN is the pairing PIN" : "pairing mode · new") : "not in pairing mode"}${f.rssi != null ? ` · ${f.rssi} dBm` : ""}</span>
      </li>`).join("");
    $$("li", list).forEach((li) => {
      const f = found[li.dataset.i];
      if (!f.setupMode) return;
      li.onclick = () => {
        provisionAddress = f.address;
        $$("li", list).forEach((x) => x.classList.toggle("selected", x === li));
        const name = $("#form-provision").elements.name;
        if (!name.value) name.value = slug(f.name);
        syncProvisionButton();
      };
    });
    const first = $$("li:not(.disabled)", list);
    if (first.length === 1) first[0].click();
  } catch (e) { status.textContent = e.message; }
}
$("#btn-scan").onclick = scan;
$("#btn-wifi-scan").onclick = () => scanWifi(true);

/** What the setup still needs; the Set up button waits for an empty list */
function provisionMissing() {
  const el = $("#form-provision").elements;
  const six = (v) => /^\d{6}$/.test(v);
  const missing = [];
  if (!provisionAddress) missing.push("pick the display in the Bluetooth list");
  if (!six(el.pairingPin.value)) missing.push("the pairing PIN (6 digits)");
  if (!/^[A-Za-z0-9_-]{1,32}$/.test(el.name.value)) missing.push("a display name (letters, digits, - or _)");
  if (!el.ssid.value.trim()) missing.push("the Wi-Fi network");
  if (!six(el.displayPin.value)) missing.push("the display PIN (6 digits)");
  else if (el.displayPinAgain.value !== el.displayPin.value) missing.push("the display PIN entered again, the same");
  return missing;
}

/**
 * Set up looks disabled until everything is there. It still takes the click (a
 * disabled button wouldn't), and the submit then says what is missing.
 */
function syncProvisionButton() {
  const go = $("#btn-provision-go");
  if (!go || go.classList.contains("busy")) return;
  const missing = provisionMissing();
  go.classList.toggle("inactive", missing.length > 0);
  go.setAttribute("aria-disabled", String(missing.length > 0));
  // Once complete, a "still needed" error from an earlier click goes away
  const err = $("#form-provision .form-error");
  if (!missing.length && err.dataset.missing) { err.hidden = true; delete err.dataset.missing; }
}

{
  const form = $("#form-provision");
  // PINs: digits only, at most 6
  for (const n of ["pairingPin", "displayPin", "displayPinAgain"]) {
    const el = form.elements[n];
    el.addEventListener("input", () => {
      const digits = el.value.replace(/\D/g, "").slice(0, 6);
      if (el.value !== digits) el.value = digits;
    });
  }
  form.addEventListener("input", syncProvisionButton);
  form.addEventListener("change", syncProvisionButton);
}

$("#form-provision").addEventListener("submit", async (e) => {
  e.preventDefault();
  const form = e.target, f = new FormData(form), err = $(".form-error", form);
  err.hidden = true;
  const body = {
    name: f.get("name").trim(), address: provisionAddress,
    pairingPin: f.get("pairingPin").trim() || null,
    ssid: f.get("ssid").trim() || null, password: f.get("password") || null,
    // One name: saved in emdx and set on the display
    acceptTerms: !!f.get("acceptTerms"), displayPin: f.get("displayPin").trim(), deviceName: f.get("name").trim(),
    orientation: f.get("orientation") || null, sleepTime: f.get("sleepTime") || null,
    networkStandby: f.get("networkStandby") === "" ? null : f.get("networkStandby") === "on",
    dailyRefresh: f.get("dailyRefresh") || null,
  };
  const missing = provisionMissing();
  if (missing.length) {
    err.textContent = `Still needed: ${missing.join(", ")}.`;
    err.dataset.missing = "1";
    err.hidden = false;
    return;
  }
  delete err.dataset.missing;

  const log = $("#provision-log"), go = $("#btn-provision-go");
  log.hidden = false;
  log.textContent = "";
  go.disabled = true;
  go.classList.add("busy");
  const line = (t) => { log.textContent += t + "\n"; log.scrollTop = log.scrollHeight; };
  let done = null;
  try {
    const res = await fetch("/api/ble/provision", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
    if (!res.ok || !res.body) throw new Error((await res.json().catch(() => ({}))).error || `${res.status}`);
    const reader = res.body.getReader();
    const dec = new TextDecoder();
    let buf = "";
    for (;;) {
      const { value, done: end } = await reader.read();
      if (end) break;
      buf += dec.decode(value, { stream: true });
      let i;
      while ((i = buf.indexOf("\n")) >= 0) {
        const msg = JSON.parse(buf.slice(0, i));
        buf = buf.slice(i + 1);
        if (msg.msg) line(msg.msg);
        if (msg.error) throw new Error(msg.error);
        if (msg.done) done = msg.done;
      }
    }
    if (!done) throw new Error("the setup stopped without finishing");
    toast(`${done.name} is set up at ${done.host}`);
    await loadDisplays(done.name);
  } catch (ex) {
    line(`❌ ${ex.message}`);
    err.textContent = ex.message;
    err.hidden = false;
  } finally { go.disabled = false; go.classList.remove("busy"); syncProvisionButton(); }
});

// ─── Boot ────────────────────────────────────────────────────────────────────

$$("[data-close]").forEach((b) => (b.onclick = () => b.closest("dialog").close()));
$("#btn-add").onclick = () => openAdd();
$("#btn-provision").onclick = openProvision;

(async () => {
  try {
    const f = await api("GET", "/api/features");
    state.bluetooth = !!f.bluetooth;
    $("#btn-provision").hidden = !state.bluetooth;
  } catch { /* older server */ }
  try { await loadDisplays(); } catch (e) { toast(e.message, "error"); }
  if (location.hash === "#setup" && state.bluetooth) openProvision();
  if (location.hash === "#add") openAdd();
  listenForThumbnails();
})();

/**
 * A display posts a thumbnail by itself after it redraws (new image, new
 * orientation...), sometimes half a minute later: show it as soon as it lands.
 * EventSource reconnects on its own if the server restarts.
 */
function listenForThumbnails() {
  if (!window.EventSource) return;
  const events = new EventSource("/api/events");
  events.addEventListener("wifi", async () => {
    const dlg = $("#dlg-provision"), select = $("#wifi-select");
    // Only an open dialog that isn't scanning itself
    if (!dlg?.open || select.disabled) return;
    try { renderWifi(await api("GET", "/api/wifi")); } catch { /* keep the current list */ }
  });
  // A saved display was found at a new address: reflect it everywhere
  events.addEventListener("displays", async () => {
    try {
      state.displays = await api("GET", "/api/displays");
      renderList();
      updateDetailHost();
    } catch { /* keep the current list */ }
  });
  events.addEventListener("thumbnail", (e) => {
    let msg = {};
    try { msg = JSON.parse(e.data); } catch { return; }
    const name = state.selected;
    if (name && (!msg.display || msg.display === name)) refreshThumb(name);
  });
}

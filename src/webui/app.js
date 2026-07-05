/* Usenet Streaming Server — embedded admin UI.
   Plain JavaScript, no dependencies. Talks to /api/v1 with the X-Api-Key
   header; the key is kept in localStorage after a successful sign-in. */

"use strict";

// ---- Tiny helpers -----------------------------------------------------------

const $ = (sel, el = document) => el.querySelector(sel);

const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );

const STORAGE_KEY = "usenet-streamer-api-key";
const state = { key: localStorage.getItem(STORAGE_KEY) || "", info: null };

function toast(message, kind = "error") {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.textContent = message;
  $("#toasts").appendChild(el);
  setTimeout(() => el.remove(), 6000);
}

function loadingHtml(label) {
  return `<div class="loading"><div class="spinner"></div>${esc(label || "Loading…")}</div>`;
}

// ---- API client -------------------------------------------------------------

async function api(path, options = {}) {
  const headers = { "X-Api-Key": state.key, ...(options.headers || {}) };
  if (options.body !== undefined) headers["Content-Type"] = "application/json";
  let res;
  try {
    res = await fetch("/api/v1" + path, { ...options, headers });
  } catch {
    throw new Error("Cannot reach the server. Is it running?");
  }
  if (res.status === 401) {
    signOut();
    throw new Error("Not authorized — please sign in again.");
  }
  if (res.status === 204) return null;
  const text = await res.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    /* non-JSON body */
  }
  if (!res.ok) {
    throw new Error((data && data.error) || `Request failed (HTTP ${res.status})`);
  }
  return data;
}

// ---- Auth / login -----------------------------------------------------------

function signOut() {
  state.key = "";
  state.info = null;
  localStorage.removeItem(STORAGE_KEY);
  $("#app").classList.add("hidden");
  $("#login").classList.remove("hidden");
  $("#login-key").value = "";
  $("#login-key").focus();
}

async function verifyKey(key) {
  let res;
  try {
    res = await fetch("/api/v1/system/info", { headers: { "X-Api-Key": key } });
  } catch {
    throw new Error("Cannot reach the server. Is it running?");
  }
  if (res.status === 401) throw new Error("That API key was not accepted.");
  if (!res.ok) throw new Error(`Server error (HTTP ${res.status}).`);
  return res.json();
}

function showApp() {
  $("#login").classList.add("hidden");
  $("#app").classList.remove("hidden");
  if (state.info) {
    $("#brand-version").textContent = `v${state.info.version}`;
  }
  navigate();
}

$("#login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const button = $("#login-submit");
  const errorBox = $("#login-error");
  errorBox.classList.add("hidden");
  button.disabled = true;
  button.textContent = "Checking…";
  try {
    const key = $("#login-key").value.trim();
    state.info = await verifyKey(key);
    state.key = key;
    localStorage.setItem(STORAGE_KEY, key);
    showApp();
  } catch (err) {
    errorBox.textContent = err.message;
    errorBox.classList.remove("hidden");
  } finally {
    button.disabled = false;
    button.textContent = "Sign in";
  }
});

$("#signout").addEventListener("click", signOut);

// ---- Modal ------------------------------------------------------------------

function openModal(html) {
  const root = $("#modal-root");
  root.innerHTML = `<div class="modal-backdrop"><div class="modal">${html}</div></div>`;
  const backdrop = $(".modal-backdrop", root);
  backdrop.addEventListener("mousedown", (e) => {
    if (e.target === backdrop) closeModal();
  });
  return $(".modal", root);
}

function closeModal() {
  $("#modal-root").innerHTML = "";
}

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeModal();
});

// ---- Router -----------------------------------------------------------------

let pageCleanup = null;

const routes = {
  dashboard: renderDashboard,
  providers: renderProviders,
  indexers: renderIndexers,
  tmdb: renderTmdb,
  preferences: renderPreferences,
  downloads: renderDownloads,
  security: renderSecurity,
};

function currentPage() {
  const page = (location.hash || "").replace(/^#\//, "");
  return routes[page] ? page : "dashboard";
}

function navigate() {
  if (!state.key) return;
  if (pageCleanup) {
    pageCleanup();
    pageCleanup = null;
  }
  closeModal();
  const page = currentPage();
  document.querySelectorAll("#nav a").forEach((a) => {
    a.classList.toggle("active", a.dataset.page === page);
  });
  const main = $("#main");
  main.innerHTML = loadingHtml();
  routes[page](main).catch((err) => {
    main.innerHTML = `<div class="card"><h2>Something went wrong</h2>
      <p class="muted">${esc(err.message)}</p></div>`;
  });
}

window.addEventListener("hashchange", navigate);

// ---- Shared page bits ---------------------------------------------------------

function pageHead(title, subtitle, actionsHtml = "") {
  return `<div class="page-head">
    <div><h1>${esc(title)}</h1><p>${subtitle}</p></div>
    <div>${actionsHtml}</div>
  </div>`;
}

function enabledChip(enabled) {
  return enabled
    ? '<span class="chip ok">enabled</span>'
    : '<span class="chip">disabled</span>';
}

function confirmDelete(label) {
  return window.confirm(`Delete ${label}? This cannot be undone.`);
}

// ---- Dashboard ----------------------------------------------------------------

async function renderDashboard(main) {
  const [info, providers, indexers, app] = await Promise.all([
    api("/system/info"),
    api("/settings/providers"),
    api("/settings/indexers"),
    api("/settings/app"),
  ]);
  state.info = info;
  $("#brand-version").textContent = `v${info.version}`;

  let healthy = false;
  try {
    healthy = (await fetch("/health")).ok;
  } catch {
    healthy = false;
  }

  const enabledProviders = providers.filter((p) => p.enabled).length;
  const enabledIndexers = indexers.filter((i) => i.enabled).length;
  const tmdbSet = !!app.tmdb_api_key;

  const item = (done, title, sub, href, linkLabel) => `
    <li>
      <span class="state ${done ? "done" : "todo"}">${done ? "✓" : "!"}</span>
      <span class="grow">
        <span class="title">${esc(title)}</span><br>
        <span class="sub">${esc(sub)}</span>
      </span>
      <a href="${href}">${esc(linkLabel)} →</a>
    </li>`;

  main.innerHTML = `
    ${pageHead("Dashboard", "Overview of your server and what still needs to be set up.")}
    <div class="dash-grid">
      <div class="card">
        <h2>Server</h2>
        <div class="kv">
          <div><span class="k">Status</span>
               <span><span class="dot ${healthy ? "ok" : "err"}"></span>${healthy ? "Healthy" : "Unreachable"}</span></div>
          <div><span class="k">Name</span><span>${esc(info.name)}</span></div>
          <div><span class="k">Version</span><span>${esc(info.version)}</span></div>
          <div><span class="k">API docs</span><span><a href="/docs" target="_blank" rel="noopener">Swagger UI ↗</a></span></div>
        </div>
      </div>
      <div class="card">
        <h2>Setup checklist</h2>
        <ul class="checklist">
          ${item(
            enabledProviders > 0,
            "Usenet provider",
            enabledProviders > 0
              ? `${enabledProviders} enabled (${providers.length} total)`
              : "Add the Usenet (NNTP) account you pay for.",
            "#/providers",
            "Providers"
          )}
          ${item(
            enabledIndexers > 0,
            "Indexer",
            enabledIndexers > 0
              ? `${enabledIndexers} enabled (${indexers.length} total)`
              : "Add a Newznab indexer so the server can find releases.",
            "#/indexers",
            "Indexers"
          )}
          ${item(
            tmdbSet,
            "TMDB API key",
            tmdbSet ? "Configured — search and artwork work." : "Needed for search, posters and metadata.",
            "#/tmdb",
            "TMDB"
          )}
        </ul>
      </div>
    </div>`;
}

// ---- Providers ----------------------------------------------------------------

async function renderProviders(main) {
  const providers = await api("/settings/providers");

  const rows = providers
    .map(
      (p) => `
      <tr data-id="${p.id}">
        <td><strong>${esc(p.name)}</strong></td>
        <td>${esc(p.host)}</td>
        <td>${p.port}</td>
        <td>${p.use_tls ? '<span class="chip accent">SSL</span>' : '<span class="chip">plain</span>'}</td>
        <td>${p.max_connections}</td>
        <td>${p.priority}</td>
        <td>${enabledChip(p.enabled)}</td>
        <td class="actions">
          <button class="btn small" data-act="test">Test</button>
          <button class="btn small" data-act="edit">Edit</button>
          <button class="btn small danger" data-act="delete">Delete</button>
          <div class="test-result hidden"></div>
        </td>
      </tr>`
    )
    .join("");

  main.innerHTML = `
    ${pageHead(
      "Usenet Providers",
      "The NNTP servers (your paid Usenet accounts) the content is streamed from. Higher priority is tried first.",
      '<button class="btn primary" id="add-provider">Add provider</button>'
    )}
    <div class="card table-wrap">
      <table>
        <thead><tr>
          <th>Name</th><th>Host</th><th>Port</th><th>SSL</th><th>Conns</th>
          <th>Priority</th><th>Status</th><th></th>
        </tr></thead>
        <tbody>${rows || '<tr><td colspan="8" class="empty">No providers yet — add your Usenet account to get started.</td></tr>'}</tbody>
      </table>
    </div>`;

  $("#add-provider").addEventListener("click", () => providerForm(null));

  main.querySelectorAll("tbody tr[data-id]").forEach((tr) => {
    const id = Number(tr.dataset.id);
    const provider = providers.find((p) => p.id === id);
    tr.querySelector('[data-act="edit"]').addEventListener("click", () => providerForm(provider));
    tr.querySelector('[data-act="delete"]').addEventListener("click", async () => {
      if (!confirmDelete(`provider "${provider.name}"`)) return;
      try {
        await api(`/settings/providers/${id}`, { method: "DELETE" });
        toast("Provider deleted.", "success");
        navigate();
      } catch (err) {
        toast(err.message);
      }
    });
    tr.querySelector('[data-act="test"]').addEventListener("click", async (e) => {
      const button = e.currentTarget;
      const box = tr.querySelector(".test-result");
      button.disabled = true;
      box.className = "test-result";
      box.textContent = "Testing…";
      try {
        const result = await api(`/settings/providers/${id}/test`, { method: "POST" });
        if (result.ok) {
          box.className = "test-result ok";
          box.textContent = `Connected · ${result.latency_ms} ms`;
        } else {
          box.className = "test-result err";
          box.textContent = result.error || "Test failed";
        }
      } catch (err) {
        box.className = "test-result err";
        box.textContent = err.message;
      } finally {
        button.disabled = false;
      }
    });
  });
}

function providerForm(provider) {
  const isNew = !provider;
  const p = provider || {
    name: "",
    host: "",
    port: 563,
    use_tls: true,
    username: "",
    password: "",
    max_connections: 10,
    priority: 0,
    enabled: true,
  };
  const modal = openModal(`
    <h2>${isNew ? "Add provider" : `Edit ${esc(p.name)}`}</h2>
    <form id="entity-form">
      <div class="form-grid">
        <div class="field wide"><label>Name</label>
          <input name="name" required value="${esc(p.name)}" placeholder="e.g. Eweka"></div>
        <div class="field wide"><label>Host</label>
          <input name="host" required value="${esc(p.host)}" placeholder="news.example.com"></div>
        <div class="field"><label>Port</label>
          <input name="port" type="number" min="1" max="65535" required value="${p.port}">
          <span class="hint">563 for SSL, 119 for plain</span></div>
        <div class="field"><label>&nbsp;</label>
          <label class="checkbox"><input type="checkbox" name="use_tls" ${p.use_tls ? "checked" : ""}> Use SSL/TLS</label></div>
        <div class="field"><label>Username</label>
          <input name="username" value="${esc(p.username || "")}" autocomplete="off"></div>
        <div class="field"><label>Password</label>
          <input name="password" type="password" value="${esc(p.password || "")}" autocomplete="new-password"></div>
        <div class="field"><label>Max connections</label>
          <input name="max_connections" type="number" min="1" max="100" required value="${p.max_connections}">
          <span class="hint">Check your provider's limit</span></div>
        <div class="field"><label>Priority</label>
          <input name="priority" type="number" required value="${p.priority}">
          <span class="hint">Higher = tried first</span></div>
        <div class="field wide">
          <label class="checkbox"><input type="checkbox" name="enabled" ${p.enabled ? "checked" : ""}> Enabled</label></div>
      </div>
      <div class="form-actions">
        <button type="button" class="btn ghost" id="cancel">Cancel</button>
        <button type="submit" class="btn primary">${isNew ? "Add provider" : "Save changes"}</button>
      </div>
    </form>`);

  $("#cancel", modal).addEventListener("click", closeModal);
  $("#entity-form", modal).addEventListener("submit", async (e) => {
    e.preventDefault();
    const f = e.target;
    const body = {
      name: f.name.value.trim(),
      host: f.host.value.trim(),
      port: Number(f.port.value),
      use_tls: f.use_tls.checked,
      username: f.username.value.trim() || null,
      password: f.password.value || null,
      max_connections: Number(f.max_connections.value),
      priority: Number(f.priority.value),
      enabled: f.enabled.checked,
    };
    try {
      if (isNew) {
        await api("/settings/providers", { method: "POST", body: JSON.stringify(body) });
        toast("Provider added.", "success");
      } else {
        await api(`/settings/providers/${provider.id}`, { method: "PUT", body: JSON.stringify(body) });
        toast("Provider saved.", "success");
      }
      closeModal();
      navigate();
    } catch (err) {
      toast(err.message);
    }
  });
}

// ---- Indexers -------------------------------------------------------------------

async function renderIndexers(main) {
  const indexers = await api("/settings/indexers");

  const rows = indexers
    .map(
      (ix) => `
      <tr data-id="${ix.id}">
        <td><strong>${esc(ix.name)}</strong></td>
        <td>${esc(ix.base_url)}</td>
        <td><code>${esc(maskTail(ix.api_key))}</code></td>
        <td>${ix.priority}</td>
        <td>${enabledChip(ix.enabled)}</td>
        <td class="actions">
          <button class="btn small" data-act="test">Test</button>
          <button class="btn small" data-act="edit">Edit</button>
          <button class="btn small danger" data-act="delete">Delete</button>
          <div class="test-result hidden"></div>
        </td>
      </tr>`
    )
    .join("");

  main.innerHTML = `
    ${pageHead(
      "Indexers",
      "Newznab-compatible search sites the server uses to find releases (e.g. NZBgeek, NZBFinder).",
      '<button class="btn primary" id="add-indexer">Add indexer</button>'
    )}
    <div class="card table-wrap">
      <table>
        <thead><tr>
          <th>Name</th><th>URL</th><th>API key</th><th>Priority</th><th>Status</th><th></th>
        </tr></thead>
        <tbody>${rows || '<tr><td colspan="6" class="empty">No indexers yet — add one so the server can find content.</td></tr>'}</tbody>
      </table>
    </div>`;

  $("#add-indexer").addEventListener("click", () => indexerForm(null));

  main.querySelectorAll("tbody tr[data-id]").forEach((tr) => {
    const id = Number(tr.dataset.id);
    const indexer = indexers.find((ix) => ix.id === id);
    tr.querySelector('[data-act="edit"]').addEventListener("click", () => indexerForm(indexer));
    tr.querySelector('[data-act="delete"]').addEventListener("click", async () => {
      if (!confirmDelete(`indexer "${indexer.name}"`)) return;
      try {
        await api(`/settings/indexers/${id}`, { method: "DELETE" });
        toast("Indexer deleted.", "success");
        navigate();
      } catch (err) {
        toast(err.message);
      }
    });
    tr.querySelector('[data-act="test"]').addEventListener("click", async (e) => {
      const button = e.currentTarget;
      const box = tr.querySelector(".test-result");
      button.disabled = true;
      box.className = "test-result";
      box.textContent = "Testing…";
      try {
        const result = await api(`/settings/indexers/${id}/test`, { method: "POST" });
        if (result.ok) {
          box.className = "test-result ok";
          box.textContent = "Connected";
        } else {
          box.className = "test-result err";
          box.textContent = result.error || "Test failed";
        }
      } catch (err) {
        box.className = "test-result err";
        box.textContent = err.message;
      } finally {
        button.disabled = false;
      }
    });
  });
}

function maskTail(secret) {
  const s = String(secret || "");
  return s.length <= 4 ? "****" : "****" + s.slice(-4);
}

function indexerForm(indexer) {
  const isNew = !indexer;
  const ix = indexer || { name: "", base_url: "", api_key: "", priority: 0, enabled: true };
  const modal = openModal(`
    <h2>${isNew ? "Add indexer" : `Edit ${esc(ix.name)}`}</h2>
    <form id="entity-form">
      <div class="form-grid">
        <div class="field wide"><label>Name</label>
          <input name="name" required value="${esc(ix.name)}" placeholder="e.g. NZBgeek"></div>
        <div class="field wide"><label>Base URL</label>
          <input name="base_url" type="url" required value="${esc(ix.base_url)}" placeholder="https://api.nzbgeek.info"></div>
        <div class="field wide"><label>API key</label>
          <input name="api_key" required value="${esc(ix.api_key)}" autocomplete="off">
          <span class="hint">Found in your account settings on the indexer's site</span></div>
        <div class="field"><label>Priority</label>
          <input name="priority" type="number" required value="${ix.priority}">
          <span class="hint">Higher = preferred</span></div>
        <div class="field"><label>&nbsp;</label>
          <label class="checkbox"><input type="checkbox" name="enabled" ${ix.enabled ? "checked" : ""}> Enabled</label></div>
      </div>
      <div class="form-actions">
        <button type="button" class="btn ghost" id="cancel">Cancel</button>
        <button type="submit" class="btn primary">${isNew ? "Add indexer" : "Save changes"}</button>
      </div>
    </form>`);

  $("#cancel", modal).addEventListener("click", closeModal);
  $("#entity-form", modal).addEventListener("submit", async (e) => {
    e.preventDefault();
    const f = e.target;
    const body = {
      name: f.name.value.trim(),
      base_url: f.base_url.value.trim(),
      api_key: f.api_key.value.trim(),
      priority: Number(f.priority.value),
      enabled: f.enabled.checked,
    };
    try {
      if (isNew) {
        await api("/settings/indexers", { method: "POST", body: JSON.stringify(body) });
        toast("Indexer added.", "success");
      } else {
        await api(`/settings/indexers/${indexer.id}`, { method: "PUT", body: JSON.stringify(body) });
        toast("Indexer saved.", "success");
      }
      closeModal();
      navigate();
    } catch (err) {
      toast(err.message);
    }
  });
}

// ---- TMDB -------------------------------------------------------------------------

async function renderTmdb(main) {
  const app = await api("/settings/app");
  main.innerHTML = `
    ${pageHead("TMDB", "The Movie Database powers search, posters and metadata. A free API key is enough.")}
    <div class="card" style="max-width:560px">
      <h2>API key</h2>
      <p class="muted" style="margin-top:0">
        Current key: ${app.tmdb_api_key ? `<code>${esc(app.tmdb_api_key)}</code>` : '<span class="chip warn">not set</span>'}
      </p>
      <form id="tmdb-form">
        <div class="field">
          <label>New TMDB API key</label>
          <input name="key" required autocomplete="off" placeholder="Paste your TMDB API key (v3)">
          <span class="hint">Create one for free at
            <a href="https://www.themoviedb.org/settings/api" target="_blank" rel="noopener">themoviedb.org/settings/api</a>
            (sign up, then request an API key — use the “API Key” value, not the long “Read Access Token”).</span>
        </div>
        <div class="form-actions">
          <button type="submit" class="btn primary">Save key</button>
        </div>
      </form>
    </div>`;

  $("#tmdb-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    try {
      await api("/settings/app", {
        method: "PUT",
        body: JSON.stringify({ tmdb_api_key: e.target.key.value.trim() }),
      });
      toast("TMDB key saved.", "success");
      navigate();
    } catch (err) {
      toast(err.message);
    }
  });
}

// ---- Preferences --------------------------------------------------------------------

const RESOLUTIONS = ["480p", "720p", "1080p", "2160p"];
const GB = 1_000_000_000;

async function renderPreferences(main) {
  const prefs = await api("/settings/preferences");

  const resOptions = (selected) =>
    RESOLUTIONS.map(
      (r) => `<option value="${r}" ${r === selected ? "selected" : ""}>${r === "2160p" ? "2160p (4K)" : r}</option>`
    ).join("");

  main.innerHTML = `
    ${pageHead(
      "Preferences",
      "How the server picks the best release when several are available. Blocked terms hard-exclude releases; preferred terms only boost the ranking."
    )}
    <form id="prefs-form" class="stack">
      <div class="card">
        <h2>Quality</h2>
        <div class="form-grid">
          <div class="field"><label>Preferred resolution</label>
            <select name="preferred_resolution">${resOptions(prefs.preferred_resolution)}</select>
            <span class="hint">The sweet spot the ranking aims for</span></div>
          <div class="field"><label>Maximum resolution</label>
            <select name="max_resolution">${resOptions(prefs.max_resolution)}</select>
            <span class="hint">Anything above is rejected</span></div>
          <div class="field"><label>Max size (GB)</label>
            <input name="max_size_gb" type="number" min="0" step="0.5"
                   value="${prefs.max_size_bytes ? (prefs.max_size_bytes / GB).toFixed(1).replace(/\.0$/, "") : ""}"
                   placeholder="no limit">
            <span class="hint">Leave empty for no limit</span></div>
          <div class="field"><label>Audio language</label>
            <input name="language" value="${esc(prefs.language)}" placeholder="en">
            <span class="hint">Two-letter code (en, de) or "original" for each title's original language</span></div>
        </div>
      </div>
      <div class="card">
        <h2>Codecs</h2>
        <div class="form-grid">
          <div class="field"><label>Preferred video codecs</label>
            <input name="preferred_video_codecs" value="${esc(prefs.preferred_video_codecs.join(", "))}" placeholder="h264, hevc">
            <span class="hint">Comma-separated, first = most preferred</span></div>
          <div class="field"><label>Preferred audio codecs</label>
            <input name="preferred_audio_codecs" value="${esc(prefs.preferred_audio_codecs.join(", "))}" placeholder="aac, ac3">
            <span class="hint">Comma-separated</span></div>
        </div>
      </div>
      <div class="card">
        <h2>Release filters</h2>
        <div class="form-grid">
          <div class="field wide"><label>Preferred terms (boost)</label>
            <input name="allowed_terms" value="${esc(prefs.allowed_terms.join(", "))}" placeholder="REMUX, PROPER">
            <span class="hint">Comma-separated — releases containing these rank higher</span></div>
          <div class="field wide"><label>Blocked terms (exclude)</label>
            <input name="blocked_terms" value="${esc(prefs.blocked_terms.join(", "))}" placeholder="CAM, TS, HDCAM">
            <span class="hint">Comma-separated — releases containing any of these are never used</span></div>
        </div>
      </div>
      <div class="form-actions" style="margin-top:0">
        <button type="submit" class="btn primary">Save preferences</button>
      </div>
    </form>`;

  $("#prefs-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const f = e.target;
    const list = (value) =>
      value
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean);
    const sizeGb = parseFloat(f.max_size_gb.value);
    const body = {
      preferred_resolution: f.preferred_resolution.value,
      max_resolution: f.max_resolution.value,
      preferred_video_codecs: list(f.preferred_video_codecs.value),
      preferred_audio_codecs: list(f.preferred_audio_codecs.value),
      max_size_bytes: Number.isFinite(sizeGb) && sizeGb > 0 ? Math.round(sizeGb * GB) : null,
      language: f.language.value.trim() || "en",
      allowed_terms: list(f.allowed_terms.value),
      blocked_terms: list(f.blocked_terms.value),
    };
    try {
      await api("/settings/preferences", { method: "PUT", body: JSON.stringify(body) });
      toast("Preferences saved.", "success");
    } catch (err) {
      toast(err.message);
    }
  });
}

// ---- Downloads -----------------------------------------------------------------------

const ACTIVE_DOWNLOAD_STATES = new Set(["queued", "pending", "downloading", "running"]);

async function renderDownloads(main) {
  main.innerHTML = `
    ${pageHead(
      "Downloads",
      "Server-side download jobs started from the Apple TV app. This list refreshes automatically."
    )}
    <div class="card table-wrap" id="downloads-card">${loadingHtml("Loading downloads…")}</div>`;

  const card = $("#downloads-card");

  async function refresh(showErrors) {
    let items;
    try {
      items = await api("/downloads");
    } catch (err) {
      if (showErrors) toast(err.message);
      return;
    }

    const chip = (status) => {
      const cls =
        status === "complete" ? "ok" : status === "failed" ? "err" : ACTIVE_DOWNLOAD_STATES.has(status) ? "accent" : "warn";
      return `<span class="chip ${cls}">${esc(status)}</span>`;
    };

    const rows = items
      .map((d) => {
        const percent = d.percent == null ? null : Math.round(d.percent);
        const active = ACTIVE_DOWNLOAD_STATES.has(d.status);
        return `
        <tr data-id="${esc(d.id)}">
          <td style="max-width:380px; overflow-wrap:anywhere"><strong>${esc(d.release_title || d.id)}</strong>
            ${d.error ? `<div class="test-result err">${esc(d.error)}</div>` : ""}</td>
          <td>${chip(d.status)}</td>
          <td>
            ${
              percent == null
                ? '<span class="muted">–</span>'
                : `<div class="progress"><div style="width:${percent}%"></div></div>
                   <div class="progress-label">${percent}%</div>`
            }
          </td>
          <td class="actions">
            <button class="btn small ${active ? "" : "danger"}" data-act="remove">${active ? "Cancel" : "Delete"}</button>
          </td>
        </tr>`;
      })
      .join("");

    card.innerHTML = `
      <table>
        <thead><tr><th>Release</th><th>Status</th><th>Progress</th><th></th></tr></thead>
        <tbody>${rows || '<tr><td colspan="4" class="empty">No downloads.</td></tr>'}</tbody>
      </table>`;

    card.querySelectorAll('[data-act="remove"]').forEach((button) => {
      button.addEventListener("click", async () => {
        const tr = button.closest("tr");
        const id = tr.dataset.id;
        const item = items.find((d) => d.id === id);
        const active = item && ACTIVE_DOWNLOAD_STATES.has(item.status);
        if (active) {
          if (!window.confirm("Cancel this download?")) return;
        } else if (!window.confirm("Remove this download from the list?")) {
          return;
        }
        let query = "";
        if (item && item.status === "complete" && item.file_path) {
          query = window.confirm("Also delete the downloaded file from disk?\n\nOK = delete file, Cancel = keep file")
            ? "?delete_file=true"
            : "";
        }
        try {
          await api(`/downloads/${id}${query}`, { method: "DELETE" });
          toast(active ? "Download cancelled." : "Download removed.", "success");
          refresh(true);
        } catch (err) {
          toast(err.message);
        }
      });
    });
  }

  await refresh(true);
  const timer = setInterval(() => refresh(false), 4000);
  pageCleanup = () => clearInterval(timer);
}

// ---- Security -------------------------------------------------------------------------

async function renderSecurity(main) {
  const app = await api("/settings/app");

  main.innerHTML = `
    ${pageHead("Security", "The API key protects every request to this server — treat it like a password.")}
    <div class="card" style="max-width:640px">
      <h2>Server API key</h2>
      <div class="kv" style="margin-bottom:18px">
        <div><span class="k">Current key</span><span><code>${esc(app.api_key)}</code></span></div>
        <div><span class="k">Rotated key active</span>
             <span>${app.api_key_override_active ? '<span class="chip ok">yes</span>' : '<span class="chip">no — using config key</span>'}</span></div>
      </div>
      <div class="notice" style="margin-bottom:18px">
        <strong>Before you change the key:</strong> every device that talks to this
        server (e.g. the Apple TV app) must be updated with the new key afterwards,
        or it will stop working. The original key from your <code>config.toml</code> /
        Docker environment always stays valid as a backup, so you cannot lock yourself out.
      </div>
      <form id="key-form">
        <div class="field">
          <label>New API key (at least 16 characters)</label>
          <input name="key" minlength="16" required autocomplete="off" placeholder="New key">
        </div>
        <div class="form-actions">
          <button type="button" class="btn" id="generate">Generate random key</button>
          <button type="submit" class="btn primary">Change key</button>
        </div>
      </form>
    </div>`;

  $("#generate").addEventListener("click", () => {
    const bytes = new Uint8Array(24);
    crypto.getRandomValues(bytes);
    const key = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
    const input = $("#key-form").key;
    input.value = key;
    input.type = "text";
  });

  $("#key-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const key = e.target.key.value.trim();
    if (key.length < 16) {
      toast("The key must be at least 16 characters long.");
      return;
    }
    if (!window.confirm("Change the server API key now? Remember to update the Apple TV app with the new key.")) {
      return;
    }
    try {
      await api("/settings/app", { method: "PUT", body: JSON.stringify({ api_key: key }) });
      // Keep this browser signed in with the new key.
      state.key = key;
      localStorage.setItem(STORAGE_KEY, key);
      toast("API key changed. Update your other devices with the new key.", "success");
      navigate();
    } catch (err) {
      toast(err.message);
    }
  });
}

// ---- Boot -----------------------------------------------------------------------------

(async function boot() {
  if (!state.key) {
    $("#login").classList.remove("hidden");
    $("#login-key").focus();
    return;
  }
  try {
    state.info = await verifyKey(state.key);
    showApp();
  } catch (err) {
    if (!err.message.includes("not accepted")) toast(err.message);
    signOut();
  }
})();

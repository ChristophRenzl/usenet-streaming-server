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
  opensubtitles: renderSubtitles,
  preferences: renderPreferences,
  users: renderUsers,
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
          ${item(
            !!app.opensubtitles_configured,
            "OpenSubtitles (optional)",
            app.opensubtitles_configured
              ? app.opensubtitles_default_key_active && app.opensubtitles_api_key_source === "default"
                ? "Using the server's built-in API key — subtitles are searched and auto-attached."
                : "Configured — subtitles are searched and auto-attached."
              : "Optional — add a key to enable automatic subtitles.",
            "#/opensubtitles",
            "Subtitles"
          )}
        </ul>
      </div>
    </div>
    <div class="card" id="streams-card" style="margin-top:16px">
      <div class="page-head" style="margin:0 0 4px">
        <div><h2 style="margin:0">Active streams</h2>
             <p class="muted" style="margin:2px 0 0">Playback sessions running right now.</p></div>
        <div><button class="btn small" id="streams-refresh">Refresh</button></div>
      </div>
      <div id="streams-body">${loadingHtml("Loading streams…")}</div>
    </div>`;

  async function loadStreams() {
    const body = $("#streams-body");
    if (!body) return;
    try {
      body.innerHTML = renderStreams(await api("/stream/sessions"));
    } catch (err) {
      body.innerHTML = `<p class="muted">${esc(err.message)}</p>`;
    }
  }
  $("#streams-refresh").addEventListener("click", loadStreams);
  await loadStreams();
  // Live-ish: refresh the streams card every 5s while the dashboard is open.
  const timer = setInterval(loadStreams, 5000);
  pageCleanup = () => clearInterval(timer);
}

function bufferedLabel(s) {
  // 6s per segment (the server's fixed HLS cadence).
  if (s.duration_secs > 0) {
    const pct = Math.min(100, Math.round(((s.segments_ready * 6) / s.duration_secs) * 100));
    return `${pct}%`;
  }
  return `${s.segments_ready} seg`;
}

function renderStreams(sessions) {
  if (!sessions.length) {
    return '<p class="muted">No active streams.</p>';
  }
  const rows = sessions
    .map((s) => {
      const scope =
        s.media_type === "tv" && s.season != null && s.episode != null
          ? `S${s.season}E${s.episode}`
          : s.media_type === "tv"
            ? "TV"
            : "Movie";
      const stateChip =
        s.state === "ready"
          ? '<span class="chip ok">ready</span>'
          : s.state === "starting"
            ? '<span class="chip accent">starting</span>'
            : s.state === "failed"
              ? '<span class="chip warn">failed</span>'
              : `<span class="chip">${esc(s.state)}</span>`;
      const work = [];
      if (s.video_transcoded) work.push("video transcode");
      if (s.audio_transcoded) work.push("audio transcode");
      const workChip = work.length
        ? `<span class="chip warn">${esc(work.join(" + "))}</span>`
        : '<span class="chip">direct copy</span>';
      return `<tr>
        <td>${esc(scope)}</td>
        <td title="${esc(s.release_title)}">${esc(s.release_title)}</td>
        <td>${stateChip}</td>
        <td>${workChip}</td>
        <td>${bufferedLabel(s)}</td>
        <td>${s.idle_secs}s ago</td>
      </tr>`;
    })
    .join("");
  return `<div class="table-wrap"><table>
    <thead><tr><th>Item</th><th>Release</th><th>State</th><th>Pipeline</th><th>Buffered</th><th>Last active</th></tr></thead>
    <tbody>${rows}</tbody></table></div>`;
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

// ---- OpenSubtitles ------------------------------------------------------------------

async function renderSubtitles(main) {
  const app = await api("/settings/app");
  const defaultKeyActive = !!app.opensubtitles_default_key_active;
  const usingDefault = defaultKeyActive && app.opensubtitles_api_key_source === "default";

  // ---- OpenSubtitles box ----
  const osConfigured = !!app.opensubtitles_configured;
  const osKeyLine = app.opensubtitles_api_key
    ? `Current key: <code>${esc(app.opensubtitles_api_key)}</code>`
    : usingDefault
      ? `Using the server's built-in API key. <span class="chip accent">server default active</span>`
      : `Current key: <span class="chip warn">not set</span>`;
  const osKeyLabel = defaultKeyActive ? "Override API key (optional)" : "OpenSubtitles API key";
  const osKeyRequired = defaultKeyActive ? "" : "required";
  const osKeyHint = defaultKeyActive
    ? `The server has a built-in key, so you only need a username/password below. Set a key here to override it with your own from <a href="https://www.opensubtitles.com/consumers" target="_blank" rel="noopener">opensubtitles.com/consumers</a>.`
    : `Get a free consumer API key at <a href="https://www.opensubtitles.com/consumers" target="_blank" rel="noopener">opensubtitles.com/consumers</a>.`;

  // ---- SubDL box ----
  const subdlSet = !!app.subdl_api_key;
  const subdlKeyLine = subdlSet
    ? `Current key: <code>${esc(app.subdl_api_key)}</code>`
    : `Current key: <span class="chip">not set</span>`;

  // ---- Provider order ----
  const order = app.subtitle_provider_order || ["opensubtitles", "subdl"];
  const providerLabel = { opensubtitles: "OpenSubtitles", subdl: "SubDL" };

  main.innerHTML = `
    ${pageHead("Subtitles", "Automatic subtitle search and delivery. Optional — playback works without it. When enabled, the server searches the providers below (in order) at playback start, matches the release, corrects fps drift, and offers the subtitle natively. Text subtitles embedded in the release are preferred (toggle below) and cost no provider quota.")}

    <div class="card" style="max-width:640px">
      <div class="page-head" style="margin:0 0 12px">
        <div><h2 style="margin:0">Provider priority</h2>
             <p class="muted" style="margin:2px 0 0">Which provider is tried first for each language.</p></div>
      </div>
      <ol id="provider-order" class="order-list">
        ${order
          .map(
            (p, i) => `<li data-provider="${esc(p)}">
              <span class="grow">${i + 1}. ${esc(providerLabel[p] || p)}</span>
              <button type="button" class="btn small" data-move="up" ${i === 0 ? "disabled" : ""}>↑</button>
              <button type="button" class="btn small" data-move="down" ${i === order.length - 1 ? "disabled" : ""}>↓</button>
            </li>`,
          )
          .join("")}
      </ol>
      <div class="form-actions">
        <button type="button" class="btn primary" id="save-order">Save order</button>
      </div>
    </div>

    <div class="card" style="max-width:640px;margin-top:16px">
      <h2>Embedded subtitles</h2>
      <p class="muted" style="margin-top:0">Text subtitles inside the release itself are extracted on the fly and offered as "(embedded)" tracks — perfectly synced and free of provider quota. Turn this off to always use the providers below instead.</p>
      <label class="field" style="display:flex;align-items:center;gap:10px;cursor:pointer">
        <input type="checkbox" id="embedded-subs" style="width:auto" ${app.embedded_subtitles_enabled === false ? "" : "checked"}>
        <span>Extract embedded subtitles</span>
      </label>
    </div>

    <div class="card" style="max-width:640px;margin-top:16px">
      <h2>OpenSubtitles</h2>
      <p class="muted" style="margin-top:0">${osConfigured ? '<span class="chip ok">enabled</span>' : '<span class="chip">not configured</span>'} ${osKeyLine}</p>
      <form id="os-key-form">
        <div class="field">
          <label>${osKeyLabel}</label>
          <input name="key" ${osKeyRequired} autocomplete="off" placeholder="${defaultKeyActive ? "Leave blank to use the server's built-in key" : "Paste your OpenSubtitles API key"}">
          <span class="hint">${osKeyHint}</span>
        </div>
        <div class="form-actions">
          <button type="submit" class="btn primary">Save key</button>
          ${app.opensubtitles_api_key ? '<button type="button" id="os-key-remove" class="btn danger">Remove key</button>' : ""}
        </div>
      </form>
      <hr class="sep">
      <h3 style="margin:0 0 8px">Account${defaultKeyActive ? "" : " (optional)"}</h3>
      <p class="muted" style="margin-top:0">
        Signing in with an OpenSubtitles account lifts the anonymous daily download quota.
        ${app.opensubtitles_username ? `Signed in as <code>${esc(app.opensubtitles_username)}</code>.` : '<span class="chip">no account set</span>'}
        ${app.opensubtitles_password_set ? '<span class="chip accent">password stored</span>' : ""}
      </p>
      <form id="os-account-form">
        <div class="field">
          <label>Username</label>
          <input name="username" autocomplete="off" placeholder="OpenSubtitles username (not e-mail)" value="${app.opensubtitles_username ? esc(app.opensubtitles_username) : ""}">
        </div>
        <div class="field">
          <label>Password</label>
          <input name="password" type="password" autocomplete="off" placeholder="Leave blank to keep the stored password">
          <span class="hint">Write-only: the stored password is never shown.</span>
        </div>
        <div class="form-actions">
          <button type="submit" class="btn primary">Save account</button>
          ${app.opensubtitles_username || app.opensubtitles_password_set ? '<button type="button" id="os-account-clear" class="btn danger">Sign out / clear account</button>' : ""}
        </div>
      </form>
    </div>

    <div class="card" style="max-width:640px;margin-top:16px">
      <h2>SubDL</h2>
      <p class="muted" style="margin-top:0">${subdlSet ? '<span class="chip ok">enabled</span>' : '<span class="chip">not configured</span>'} ${subdlKeyLine}</p>
      <form id="subdl-key-form">
        <div class="field">
          <label>SubDL API key</label>
          <input name="key" autocomplete="off" placeholder="Paste your SubDL API key">
          <span class="hint">Get a free API key at <a href="https://subdl.com/panel/api" target="_blank" rel="noopener">subdl.com/panel/api</a>.</span>
        </div>
        <div class="form-actions">
          <button type="submit" class="btn primary">Save key</button>
          ${subdlSet ? '<button type="button" id="subdl-key-remove" class="btn danger">Remove key</button>' : ""}
        </div>
      </form>
    </div>`;

  // ---- Embedded subtitles toggle ----
  $("#embedded-subs").addEventListener("change", async (e) => {
    try {
      await api("/settings/app", {
        method: "PUT",
        body: JSON.stringify({ embedded_subtitles_enabled: e.target.checked }),
      });
      toast(e.target.checked ? "Embedded subtitles enabled." : "Embedded subtitles disabled.", "success");
    } catch (err) {
      e.target.checked = !e.target.checked;
      toast("Saving failed: " + err.message);
    }
  });

  // ---- Provider order interactions ----
  const orderList = $("#provider-order");
  orderList.querySelectorAll("[data-move]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const li = btn.closest("li");
      if (btn.dataset.move === "up" && li.previousElementSibling) {
        li.parentNode.insertBefore(li, li.previousElementSibling);
      } else if (btn.dataset.move === "down" && li.nextElementSibling) {
        li.parentNode.insertBefore(li.nextElementSibling, li);
      }
    });
  });
  $("#save-order").addEventListener("click", async () => {
    const providers = Array.from(orderList.querySelectorAll("li")).map(
      (li) => li.dataset.provider,
    );
    try {
      await api("/settings/app", {
        method: "PUT",
        body: JSON.stringify({ subtitle_provider_order: providers }),
      });
      toast("Provider order saved.", "success");
      navigate();
    } catch (err) {
      toast(err.message);
    }
  });

  // ---- OpenSubtitles key ----
  $("#os-key-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const key = e.target.key.value.trim();
    if (defaultKeyActive && !key) {
      toast("Using the server's built-in API key.", "success");
      return;
    }
    try {
      await api("/settings/app", { method: "PUT", body: JSON.stringify({ opensubtitles_api_key: key }) });
      toast("OpenSubtitles key saved.", "success");
      navigate();
    } catch (err) {
      toast(err.message);
    }
  });
  const osRemove = $("#os-key-remove");
  if (osRemove) {
    osRemove.addEventListener("click", async () => {
      if (!confirm("Remove the stored OpenSubtitles API key?")) return;
      try {
        await api("/settings/app", { method: "PUT", body: JSON.stringify({ opensubtitles_api_key: "" }) });
        toast("OpenSubtitles key removed.", "success");
        navigate();
      } catch (err) {
        toast(err.message);
      }
    });
  }

  // ---- OpenSubtitles account ----
  $("#os-account-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const body = { opensubtitles_username: e.target.username.value.trim() };
    const password = e.target.password.value;
    if (password) body.opensubtitles_password = password;
    try {
      await api("/settings/app", { method: "PUT", body: JSON.stringify(body) });
      toast("OpenSubtitles account saved.", "success");
      navigate();
    } catch (err) {
      toast(err.message);
    }
  });
  const osAccountClear = $("#os-account-clear");
  if (osAccountClear) {
    osAccountClear.addEventListener("click", async () => {
      if (!confirm("Sign out and clear the stored OpenSubtitles username and password?")) return;
      try {
        await api("/settings/app", {
          method: "PUT",
          body: JSON.stringify({ opensubtitles_username: "", opensubtitles_password: "" }),
        });
        toast("OpenSubtitles account cleared.", "success");
        navigate();
      } catch (err) {
        toast(err.message);
      }
    });
  }

  // ---- SubDL key ----
  $("#subdl-key-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const key = e.target.key.value.trim();
    if (!key) {
      toast("Enter a SubDL API key.");
      return;
    }
    try {
      await api("/settings/app", { method: "PUT", body: JSON.stringify({ subdl_api_key: key }) });
      toast("SubDL key saved.", "success");
      navigate();
    } catch (err) {
      toast(err.message);
    }
  });
  const subdlRemove = $("#subdl-key-remove");
  if (subdlRemove) {
    subdlRemove.addEventListener("click", async () => {
      if (!confirm("Remove the stored SubDL API key?")) return;
      try {
        await api("/settings/app", { method: "PUT", body: JSON.stringify({ subdl_api_key: "" }) });
        toast("SubDL key removed.", "success");
        navigate();
      } catch (err) {
        toast(err.message);
      }
    });
  }
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
          <div class="field"><label>Prefer larger releases</label>
            <label class="check"><input type="checkbox" name="prefer_larger_releases" ${prefs.prefer_larger_releases ? "checked" : ""}> Rank bigger files first</label>
            <span class="hint">More bitrate at the same resolution; needs a fast connection</span></div>
          <div class="field"><label>Allow Dolby Vision</label>
            <label class="check"><input type="checkbox" name="allow_dolby_vision" ${prefs.allow_dolby_vision !== false ? "checked" : ""}> Use DV-only releases</label>
            <span class="hint">Off: DV-only releases are skipped and stray DV streams are tone-mapped</span></div>
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
      prefer_larger_releases: f.prefer_larger_releases.checked,
      allow_dolby_vision: f.allow_dolby_vision.checked,
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

/// "3h 42m" (or "12m" / "—") from a seconds count.
function formatWatchTime(secs) {
  if (!secs || secs < 60) return "\u2014";
  const h = Math.floor(secs / 3600);
  const m = Math.round((secs % 3600) / 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

/// Relative "3h ago" / "2d ago" from an SQLite UTC timestamp, or "never".
function formatLastActivity(ts) {
  if (!ts) return "never";
  const then = new Date(ts.replace(" ", "T") + "Z");
  const mins = Math.floor((Date.now() - then.getTime()) / 60000);
  if (Number.isNaN(mins)) return esc(ts);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  if (mins < 48 * 60) return `${Math.floor(mins / 60)}h ago`;
  return `${Math.floor(mins / 1440)}d ago`;
}

async function renderUsers(main) {
  const users = await api("/users");

  const rows = users
    .map((u) => {
      const role = u.is_admin
        ? '<span class="chip ok">admin</span>'
        : '<span class="chip">user</span>';
      const owner = u.id === 1 ? '<span class="chip">owner</span>' : "";
      const passwordState = u.has_password
        ? ""
        : '<span class="chip">no password set</span>';
      const actions =
        u.id === 1
          ? '<span class="chip">API-key access</span>'
          : `<button class="btn small" data-pw="${u.id}" data-name="${esc(u.name)}">Reset password</button>
             <button class="btn small danger" data-del="${u.id}" data-name="${esc(u.name)}">Delete</button>`;
      return `<tr>
        <td>${esc(u.name)} ${owner}</td>
        <td>${role} ${passwordState}</td>
        <td>${formatWatchTime(u.watch_time_secs)}</td>
        <td class="muted">${formatLastActivity(u.last_activity)}</td>
        <td class="actions">${actions}</td>
      </tr>`;
    })
    .join("");

  main.innerHTML = `
    ${pageHead(
      "Users",
      "Accounts that can sign in on the web and mobile apps. Each user has their own watch history and watchlist; the API key always acts as the owner.",
      '<button class="btn primary" id="add-user">Add user</button>',
    )}
    <div class="card">
      <table>
        <thead><tr><th>Name</th><th>Role</th><th>Watch time</th><th>Last activity</th><th></th></tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>
    <div class="notice" style="margin-top:18px;max-width:640px">
      The <strong>owner</strong> (first account) is what the server API key
      authenticates as — it cannot be deleted and has no password. Devices
      either use the API key (owner) or sign in with one of the user accounts
      below.
    </div>`;

  $("#add-user").addEventListener("click", () => {
    const modal = openModal(`
      <h2>Add user</h2>
      <form id="user-form">
        <div class="field">
          <label>Username</label>
          <input name="username" required autocomplete="off" placeholder="e.g. anna">
        </div>
        <div class="field">
          <label>Password (at least 4 characters)</label>
          <input name="password" type="password" required minlength="4" autocomplete="new-password">
        </div>
        <label class="checkbox"><input type="checkbox" name="is_admin"> Administrator (can manage users)</label>
        <div class="form-actions">
          <button type="button" class="btn" id="cancel-user">Cancel</button>
          <button type="submit" class="btn primary">Create</button>
        </div>
      </form>`);
    $("#cancel-user", modal).addEventListener("click", closeModal);
    $("#user-form", modal).addEventListener("submit", async (e) => {
      e.preventDefault();
      const form = e.target;
      try {
        await api("/users", {
          method: "POST",
          body: JSON.stringify({
            username: form.username.value.trim(),
            password: form.password.value,
            is_admin: form.is_admin.checked,
          }),
        });
        closeModal();
        toast("User created.", "success");
        navigate();
      } catch (err) {
        toast(err.message);
      }
    });
  });

  main.querySelectorAll("[data-del]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      if (!confirmDelete(`user "${btn.dataset.name}" and their watch data`)) return;
      try {
        await api(`/users/${btn.dataset.del}`, { method: "DELETE" });
        toast("User deleted.", "success");
        navigate();
      } catch (err) {
        toast(err.message);
      }
    });
  });

  main.querySelectorAll("[data-pw]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const modal = openModal(`
        <h2>Reset password for ${esc(btn.dataset.name)}</h2>
        <p class="muted">All signed-in sessions of this user are logged out.</p>
        <form id="pw-form">
          <div class="field">
            <label>New password (at least 4 characters)</label>
            <input name="password" type="password" required minlength="4" autocomplete="new-password">
          </div>
          <div class="form-actions">
            <button type="button" class="btn" id="cancel-pw">Cancel</button>
            <button type="submit" class="btn primary">Reset password</button>
          </div>
        </form>`);
      $("#cancel-pw", modal).addEventListener("click", closeModal);
      $("#pw-form", modal).addEventListener("submit", async (e) => {
        e.preventDefault();
        try {
          await api(`/users/${btn.dataset.pw}/password`, {
            method: "PUT",
            body: JSON.stringify({ password: e.target.password.value }),
          });
          closeModal();
          toast("Password reset - the user was signed out everywhere.", "success");
        } catch (err) {
          toast(err.message);
        }
      });
    });
  });
}

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

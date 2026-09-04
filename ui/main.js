const { core, dialog, event } = window.__TAURI__;
const invoke = core.invoke;
const convertFileSrc = core.convertFileSrc;

const $ = (id) => document.getElementById(id);

let mode = "local";
let lastState = null;
let seekDragging = false;
let seekDragValue = 0;
let volumeDragging = false;
let currentYtTracks = [];
let queueInfo = null;
let trackList = [];
let trackListKey = null;
let lyricsLines = [];
let lyricsActiveIdx = -1;
let isFav = false;
let sortMode = 0;
let filterText = "";

function showError(message) {
  $("sidebar-error").textContent = String(message);
  $("sidebar-error").classList.remove("hidden");
}

function hideError() {
  $("sidebar-error").classList.add("hidden");
}

/* ---------- modes ---------- */

function setMode(m) {
  mode = m;
  $("side-local").classList.toggle("hidden", mode !== "local");
  $("side-youtube").classList.toggle("hidden", mode !== "youtube");
  $("mode-local").classList.toggle("active", mode === "local");
  $("mode-youtube").classList.toggle("active", mode === "youtube");
  const showShuffle = mode === "youtube";
  $("btn-shuffle").style.display = showShuffle ? "" : "none";
  $("lh-shuffle").style.display = showShuffle ? "" : "none";
  showMainView("player");
}

function showMainView(name) {
  document.querySelectorAll(".view").forEach((v) => v.classList.add("hidden"));
  $("view-" + name).classList.remove("hidden");
  $("mode-settings").classList.toggle("active", name === "settings");
}

$("mode-local").onclick = () => setMode("local");
$("mode-youtube").onclick = () => setMode("youtube");
$("mode-settings").onclick = () => {
  if ($("view-settings").classList.contains("hidden")) showMainView("settings");
  else showMainView("player");
};

/* ---------- splitters ---------- */

function initSplitter(splitterId, getEl, apply, min, max, storageKey) {
  const el = $(splitterId);
  if (!el) return;
  const saved = localStorage.getItem(storageKey);
  if (saved) apply(Number(saved));

  el.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    const startX = e.clientX;
    const startY = e.clientY;
    const startSize = getEl().getBoundingClientRect();
    const startW = startSize.width;
    const startH = startSize.height;

    const move = (ev) => {
      if (splitterId === "splitter") {
        apply(Math.min(max, Math.max(min, startW + ev.clientX - startX)));
      } else {
        apply(Math.min(max, Math.max(min, startH + ev.clientY - startY)));
      }
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      const rect = getEl().getBoundingClientRect();
      localStorage.setItem(storageKey, String(Math.round(splitterId === "splitter" ? rect.width : rect.height)));
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  });
}

initSplitter(
  "splitter",
  () => $("sidebar"),
  (w) => { $("sidebar").style.width = w + "px"; },
  220,
  640,
  "sidebarW"
);

function fmt(secs) {
  if (!Number.isFinite(secs) || secs < 0) secs = 0;
  const s = Math.floor(secs);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const r = s % 60;
  const mm = String(m).padStart(2, "0");
  const rr = String(r).padStart(2, "0");
  return h > 0 ? `${h}:${mm}:${rr}` : `${mm}:${rr}`;
}

function escapeHtml(text) {
  return String(text)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

/* ---------- list header (playlist cover + actions) ---------- */

function setListHeader(meta) {
  const h = $("list-header");
  if (!meta) {
    h.classList.add("hidden");
    return;
  }
  h.classList.remove("hidden");
  $("lh-title").textContent = meta.title || "";
  $("lh-subtitle").textContent = meta.subtitle || "";
  const cover = $("lh-cover");
  if (meta.cover) {
    if (cover.dataset.src !== meta.cover) {
      cover.dataset.src = meta.cover;
      cover.src = meta.cover;
    }
    cover.classList.remove("hidden");
    $("lh-placeholder").classList.add("hidden");
  } else {
    cover.dataset.src = "";
    cover.classList.add("hidden");
    $("lh-placeholder").classList.remove("hidden");
  }
}

$("lh-play").onclick = () => {
  if (mode === "youtube") {
    if (currentYtTracks.length) playYtAt(0);
  } else {
    invoke("play_track", { index: 0 }).then(refresh);
  }
};

$("lh-shuffle").onclick = () => {
  if (mode === "youtube") shuffleYt();
};

/* ---------- filter + sort ---------- */

function applyFilterSort(rows) {
  let out = rows.map((r, i) => ({ ...r, orig: i }));
  if (filterText) {
    const q = filterText.toLowerCase();
    out = out.filter(
      (r) =>
        r.title.toLowerCase().includes(q) ||
        (r.artist || "").toLowerCase().includes(q)
    );
  }
  if (sortMode === 1) out.sort((a, b) => a.title.localeCompare(b.title));
  else if (sortMode === 2) out.sort((a, b) => (a.artist || "").localeCompare(b.artist || ""));
  else if (sortMode === 3) out.sort((a, b) => a.duration - b.duration);
  return out;
}

$("lh-filter").addEventListener("input", () => {
  filterText = $("lh-filter").value;
  refresh();
});

const SORT_LABELS = ["default", "title", "artist", "duration"];
$("lh-sort").onclick = () => {
  sortMode = (sortMode + 1) % SORT_LABELS.length;
  $("lh-sort").textContent = "Sort: " + SORT_LABELS[sortMode];
  refresh();
};

/* ---------- local playlists ---------- */

function renderPlaylists(library) {
  const el = $("playlists");
  el.innerHTML = "";
  library.forEach((pl, i) => {
    const div = document.createElement("div");
    div.className = "item";
    div.innerHTML = `<span>${escapeHtml(pl.name)}</span> <span class="count">(${pl.tracks})</span>`;
    div.onclick = async () => {
      try {
        hideError();
        const infos = await invoke("open_playlist", { index: i });
        trackList = infos.map((t) => ({
          title: t.title,
          artist: t.artist,
          duration: t.duration,
        }));
        trackListKey = null;
        const coverPath = (infos.find((t) => t.cover) || {}).cover;
        setListHeader({
          title: pl.name,
          subtitle: `${infos.length} tracks`,
          cover: coverPath ? convertFileSrc(coverPath) : "",
        });
        refresh();
      } catch (e) {
        showError(e);
      }
    };
    el.appendChild(div);
  });
}

async function loadLibrary() {
  try {
    renderPlaylists(await invoke("get_library"));
  } catch (e) {
    showError(e);
  }
}

event.listen("library-loaded", () => loadLibrary());

/* ---------- youtube lists ---------- */

function playYtAt(i) {
  const items = currentYtTracks.map((t) => ({
    id: t.video_id,
    title: t.title,
    artist: t.artist,
    thumbnail: t.thumbnail || "",
    durationSecs: t.duration_secs,
  }));
  invoke("play_queue", { items, start: i })
    .then(() => refresh())
    .catch((e) => showError(e));
}

// Show the given tracks on the right side (search results, liked songs,
// user playlist contents). Playback is not started.
let ytRows = [];
let ytListVersion = 0;

function setYtTracks(tracks, append, meta) {
  currentYtTracks = append ? currentYtTracks.concat(tracks) : tracks;
  if (!append) ytListVersion++;
  ytRows = currentYtTracks.map((t) => ({
    title: t.title,
    artist: t.artist,
    duration: t.duration_secs,
  }));
  if (meta) {
    setListHeader(meta);
  }
  refresh();
}

let searchQuery = "";
let searchLimit = 20;

async function doSearch(append) {
  const query = $("yt-query").value.trim();
  if (!query) return;
  if (!append) {
    searchQuery = query;
    searchLimit = 20;
  }
  const music = $("yt-music").checked;
  try {
    const tracks = await invoke("search_youtube", { query: searchQuery, limit: searchLimit, music });
    setYtTracks(tracks, append, append ? null : {
      title: `Search: ${searchQuery}`,
      subtitle: music ? "YouTube Music" : "YouTube",
      cover: tracks[0] ? tracks[0].thumbnail || "" : "",
    });
    $("yt-more-btn").classList.remove("hidden");
    if (append) searchLimit += 20;
  } catch (e) {
    showError(e);
  }
}

$("yt-search-btn").onclick = () => doSearch(false);
$("yt-more-btn").onclick = () => doSearch(true);
$("yt-query").addEventListener("keydown", (e) => {
  if (e.key === "Enter") doSearch(false);
});

function shuffleYt() {
  if (!currentYtTracks.length) return;
  const arr = currentYtTracks.slice();
  for (let i = arr.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [arr[i], arr[j]] = [arr[j], arr[i]];
  }
  currentYtTracks = arr;
  ytListVersion++;
  refresh();
  playYtAt(0);
}

$("btn-shuffle").onclick = shuffleYt;

async function renderYtTrackList(invokeName) {
  try {
    hideError();
    const tracks = await invoke(invokeName);
    setYtTracks(tracks, false, {
      title: invokeName === "get_liked_songs" ? "Liked songs" : "Tracks",
      subtitle: `${tracks.length} tracks`,
      cover: tracks[0] ? tracks[0].thumbnail || "" : "",
    });
    $("yt-more-btn").classList.add("hidden");
  } catch (e) {
    showError(e);
  }
}

$("yt-liked-btn").onclick = () => renderYtTrackList("get_liked_songs");

$("yt-user-btn").onclick = async () => {
  try {
    const playlists = await invoke("get_user_playlists");
    const el = $("yt-user-playlists");
    el.innerHTML = "";
    playlists.forEach((p) => {
      const div = document.createElement("div");
      div.className = "item";
      const thumb = p.thumbnail
        ? `<img class="yt-thumb" src="${escapeHtml(p.thumbnail)}" loading="lazy" alt="" />`
        : "";
      div.innerHTML = `${thumb}<span>${escapeHtml(p.name)}</span> <span class="count">(${p.track_count})</span>`;
      div.onclick = async () => {
        try {
          hideError();
          const tracks = await invoke("get_playlist_tracks", { playlistId: p.id });
          setYtTracks(tracks, false, {
            title: p.name,
            subtitle: `${p.track_count} tracks`,
            cover: p.thumbnail || (tracks[0] ? tracks[0].thumbnail || "" : ""),
          });
          $("yt-more-btn").classList.add("hidden");
        } catch (e) {
          showError(e);
        }
      };
      el.appendChild(div);
    });
  } catch (e) {
    showError(e);
  }
};

/* ---------- auth ---------- */

function setAuthStatus(text, ok) {
  const el = $("yt-auth-status");
  el.textContent = text;
  el.className = "auth-status" + (ok === true ? " ok" : ok === false ? " no" : "");
}

$("yt-auth-start").onclick = async () => {
  const clientId = $("yt-auth-cid").value.trim();
  const clientSecret = $("yt-auth-csec").value.trim();
  if (!clientId || !clientSecret) {
    showError("Client ID / Secret from Google Cloud Console. Client type must be 'TVs and Limited Input devices'");
    return;
  }
  try {
    const resp = await invoke("start_oauth", { clientId, clientSecret });
    if (!resp.ok) {
      showError(resp.error);
      return;
    }
    $("yt-auth-link").href = resp.url;
    $("yt-auth-link").textContent = resp.url;
    $("yt-auth-url").classList.remove("hidden");
    showError("Sign in in the browser, then this app finishes automatically");
    pollOauthUntilDone();
  } catch (e) {
    showError(e);
  }
};

async function pollOauthUntilDone() {
  try {
    const resp = await invoke("poll_oauth");
    if (resp.done) {
      hideError();
      $("yt-auth-url").classList.add("hidden");
      setAuthStatus("Signed in", true);
      return;
    }
    showError(resp.error || "oauth failed");
  } catch (e) {
    showError(e);
  }
}

$("yt-browser-auth-btn").onclick = async () => {
  const text = $("yt-auth-headers").value.trim();
  if (!text) {
    showError("Вставь заголовки запроса из браузера");
    return;
  }
  try {
    await invoke("set_browser_auth", { authText: text });
    hideError();
    setAuthStatus("Signed in", true);
  } catch (e) {
    showError(e);
  }
};

/* ---------- audio quality ---------- */

async function loadSettings() {
  try {
    const s = await invoke("get_settings");
    const radio = document.querySelector(`input[name="quality"][value="${s.audio_quality}"]`);
    if (radio) radio.checked = true;
    const norm = document.querySelector(`input[name="normlevel"][value="${s.normalize_level || "medium"}"]`);
    if (norm) norm.checked = true;
  } catch (e) {
    showError(e);
  }
}

document.querySelectorAll('input[name="normlevel"]').forEach((r) => {
  r.onchange = async () => {
    if (!r.checked) return;
    try {
      await invoke("set_normalize_level", { level: r.value });
    } catch (e) {
      showError(e);
    }
  };
});

document.querySelectorAll('input[name="quality"]').forEach((r) => {
  r.onchange = async () => {
    if (!r.checked) return;
    try {
      await invoke("set_audio_quality", { quality: r.value });
    } catch (e) {
      showError(e);
    }
  };
});

$("btn-clear-cache").onclick = async () => {
  const status = $("cache-status");
  try {
    await invoke("clear_cache");
    status.textContent = "Cache cleared";
  } catch (e) {
    status.textContent = "";
    showError(e);
  }
};

/* ---------- transport ---------- */

$("open-folder").onclick = async () => {
  try {
    const path = await dialog.open({ directory: true, title: "Choose music folder" });
    if (!path) return;
    hideError();
    renderPlaylists(await invoke("open_folder", { path }));
    setMode("local");
  } catch (e) {
    showError(e);
  }
};

$("btn-play").onclick = () => invoke("toggle_pause").then(refresh);
$("btn-prev").onclick = () => invoke("prev_track").then(refresh).catch((e) => showError(e));
$("btn-next").onclick = () => invoke("next_track").then(refresh).catch((e) => showError(e));
$("btn-stop").onclick = () => invoke("stop").then(refresh);

function renderRepeat(mode_) {
  const btn = $("btn-repeat");
  if (mode_ === "all") {
    btn.textContent = "↻";
    btn.className = "tbtn repeat on";
    btn.title = "Repeat: all";
  } else if (mode_ === "one") {
    btn.textContent = "↻1";
    btn.className = "tbtn repeat one";
    btn.title = "Repeat: one";
  } else {
    btn.textContent = "↻";
    btn.className = "tbtn repeat";
    btn.title = "Repeat: off";
  }
}

$("btn-repeat").onclick = async () => {
  try {
    renderRepeat(await invoke("cycle_repeat"));
  } catch (e) {
    showError(e);
  }
};

seekEl = $("seek");
seekEl.addEventListener("pointerdown", () => {
  seekDragging = true;
});
seekEl.addEventListener("input", () => {
  seekDragValue = Number(seekEl.value);
  $("time-elapsed").textContent = fmt(seekDragValue);
});
seekEl.addEventListener("change", () => {
  invoke("seek", { secs: seekDragValue });
  seekDragging = false;
});

const volumeEl = $("volume");
volumeEl.addEventListener("pointerdown", () => {
  volumeDragging = true;
});
volumeEl.addEventListener("input", () => {
  const v = Number(volumeEl.value) / 100;
  $("volume-value").textContent = volumeEl.value + "%";
  invoke("set_volume", { volume: v }).catch(() => {});
});
volumeEl.addEventListener("change", () => {
  volumeDragging = false;
});

/* ---------- main rendering ---------- */

const ROW_H = 46;
const OVERSCAN = 8;
let renderedTracksKey = null;
let trackRows = [];
let trackActive = -1;
let lastFirst = -1;

function buildRow(row, i) {
  const div = document.createElement("div");
  const isActive = row.orig === trackActive;
  div.className = "trow" + (isActive ? " active" : "");
  const dur = row.duration > 0 ? fmt(row.duration) : "";
  div.innerHTML =
    (isActive
      ? `<div class="t-idx"><span class="eq"><i></i><i></i><i></i></span></div>`
      : `<div class="t-idx">${i + 1}</div>`) +
    `<div class="t-title">${escapeHtml(row.title)}</div>` +
    `<div class="t-artist">${escapeHtml(row.artist || "")}</div>` +
    `<div class="t-dur">${dur}</div>`;
  div.onclick = () => {
    const orig = row.orig;
    if (mode === "youtube") {
      if (queueInfo && queueInfo.active && queueInfo.items.length) {
        // Jump within the currently playing queue (same order).
        invoke("play_queue", { items: queueInfo.items, start: orig })
          .then(refresh)
          .catch((e) => showError(e));
      } else {
        playYtAt(orig);
      }
    } else {
      invoke("play_track", { index: orig }).then(refresh);
    }
  };
  return div;
}

function updateTracks() {
  const el = $("tracks");
  const savedScroll = el.scrollTop;
  el.innerHTML = "";
  const total = trackRows.length;
  if (!total) {
    lastFirst = -1;
    return;
  }
  const first = Math.max(0, Math.floor(savedScroll / ROW_H) - OVERSCAN);
  const visible = Math.ceil(el.clientHeight / ROW_H) + OVERSCAN * 2;
  const count = Math.min(total - first, visible);
  lastFirst = first;
  const top = document.createElement("div");
  top.style.height = first * ROW_H + "px";
  el.appendChild(top);
  for (let i = first; i < first + count; i++) {
    el.appendChild(buildRow(trackRows[i], i));
  }
  const bottom = document.createElement("div");
  bottom.style.height = (total - first - count) * ROW_H + "px";
  el.appendChild(bottom);
  el.scrollTop = savedScroll;
}

$("tracks").addEventListener("scroll", () => {
  const first = Math.floor($("tracks").scrollTop / ROW_H);
  if (Math.abs(first - lastFirst) >= OVERSCAN / 2) updateTracks();
});

// Virtualized track list: only the visible rows exist in the DOM, so huge
// playlists stay cheap regardless of their size.
function renderTracks(rows, activeIndex, key) {
  if (key !== renderedTracksKey) {
    renderedTracksKey = key;
    trackRows = rows;
    trackActive = activeIndex;
    $("tracks").scrollTop = 0;
    updateTracks();
    return;
  }
  if (activeIndex !== trackActive) {
    trackActive = activeIndex;
    updateTracks();
  }
}

async function refreshTracks(key, activeIndex) {
  if (key === trackListKey) {
    renderTracks(applyFilterSort(trackList), activeIndex, key + ":" + filterText + ":" + sortMode);
    return;
  }
  try {
    const list = await invoke("get_track_list");
    trackList = list.map((t) => ({
      title: t.title,
      artist: t.artist,
      duration: t.duration,
    }));
    trackListKey = key;
    renderTracks(applyFilterSort(trackList), activeIndex, key + ":" + filterText + ":" + sortMode);
  } catch (e) {
    showError(e);
  }
}

function renderState(state) {
  lastState = state;
  const hasYtList = mode === "youtube" && currentYtTracks.length > 0;
  if (state || hasYtList) {
    $("empty").classList.add("hidden");
    $("player").classList.remove("hidden");
    $("player-bar").classList.remove("hidden");
  }

  const qActive = queueInfo && queueInfo.active;
  const currentItem = qActive ? queueInfo.items[queueInfo.current] : null;

  let npTitle = "Nothing playing";
  let npArtist = "";
  let coverSrc = "";

  if (mode === "youtube") {
    if (qActive && currentItem) {
      npTitle = currentItem.title;
      npArtist = currentItem.artist;
      coverSrc = currentItem.thumbnail || "";
    } else if (currentYtTracks.length) {
      npTitle = currentYtTracks[0].title;
      npArtist = currentYtTracks[0].artist;
      coverSrc = currentYtTracks[0].thumbnail || "";
    } else if (state && state.track) {
      npTitle = state.track.title;
      npArtist = state.track.artist;
    }
  } else if (state && state.track) {
    npTitle = state.track.title;
    npArtist = state.track.artist;
    if (state.track.cover) coverSrc = convertFileSrc(state.track.cover);
  } else if (state) {
    npTitle = state.playlist_name || "Nothing playing";
  }

  $("np-title").textContent = npTitle;
  $("np-artist").textContent = npArtist;

  const status = state ? state.status : "stopped";
  const statusEl = $("np-status");
  statusEl.textContent = status;
  statusEl.className = "np-status" + (status !== "stopped" ? " " + status : "");

  $("btn-play").textContent = status === "playing" ? "⏸" : "▶";

  if (state && state.speed) {
    const idx = SPEEDS.indexOf(state.speed);
    if (idx >= 0) speedIdx = idx;
    $("btn-speed").textContent = state.speed + "x";
  }

  const cover = $("np-cover");
  if (coverSrc) {
    if (cover.dataset.src !== coverSrc) {
      cover.dataset.src = coverSrc;
      cover.src = coverSrc;
    }
    cover.classList.remove("hidden");
    $("np-placeholder").classList.add("hidden");
  } else {
    cover.dataset.src = "";
    cover.classList.add("hidden");
    $("np-placeholder").classList.remove("hidden");
  }

  const total = state ? state.total : 0;
  const elapsed = state ? state.elapsed : 0;
  $("time-total").textContent = fmt(total);
  if (!seekDragging) {
    seekEl.max = total > 0 ? total : 100;
    seekEl.value = elapsed;
    $("time-elapsed").textContent = fmt(elapsed);
  }

  if (state && !volumeDragging) {
    volumeEl.value = Math.round(state.volume * 100);
    $("volume-value").textContent = Math.round(state.volume * 100) + "%";
  }

  if (mode === "youtube") {
    // Show the actual playing queue whenever one is active, so the list
    // always matches what is playing (including after shuffle / auto-advance).
    const queueActive = queueInfo && queueInfo.active && queueInfo.items.length > 0;
    let rows, activeIdx, key;
    if (queueActive) {
      rows = queueInfo.items.map((it) => ({
        title: it.title,
        artist: it.artist,
        duration: it.duration_secs || 0,
      }));
      activeIdx = queueInfo.current;
      key = "q:" + (queueInfo.version || 0);
      $("lh-related").classList.remove("hidden");
    } else {
      rows = ytRows;
      activeIdx = -1;
      key = "y:" + ytListVersion;
      $("lh-related").classList.add("hidden");
    }
    renderTracks(applyFilterSort(rows), activeIdx, key + ":" + filterText + ":" + sortMode);
  } else if (state) {
    const key = "l:" + state.playlist_name + ":" + state.playlist_len;
    $("lh-related").classList.add("hidden");
    refreshTracks(key, state.idx);
  }
}

let queueVersion = null;

async function refreshQueue() {
  try {
    const v = await invoke("get_queue_version");
    if (v === queueVersion) return;
    queueVersion = v;
    queueInfo = await invoke("get_queue_info");
    renderRepeat(queueInfo.repeat);
    if (!$("queue-panel").classList.contains("hidden")) renderQueuePanel();
  } catch (e) {
    showError(e);
  }
}

async function refresh() {
  try {
    await refreshQueue();
    renderState(await invoke("get_state"));
  } catch (e) {
    showError(e);
  }
}

function renderApiStatus(s) {
  if (!s) return;
  const text = "API: " + (s.state === "ok" ? "ok v" + s.version : s.state);
  const cls = s.state === "ok" ? "ok" : s.state === "down" ? "down" : "starting";
  const el = $("api-badge-side");
  el.textContent = text;
  el.className = "badge api-side " + cls;

  if (s.state === "ok") {
    setAuthStatus(s.auth_ok ? "Signed in" : "Not signed in", s.auth_ok);
  }
}

function renderDownloadProgress(p) {
  const block = $("yt-progress");
  const bar = $("yt-progress-bar");
  const text = $("yt-progress-text");
  if (!p || p.done) {
    block.classList.add("hidden");
    return;
  }
  block.classList.remove("hidden");
  const pct = p.total > 0 ? Math.round((p.bytes / p.total) * 100) : 0;
  bar.style.width = (p.total > 0 ? (p.bytes / p.total) * 100 : 30) + "%";
  text.textContent =
    "Downloading: " + p.title + " " + pct + "% (" +
    (p.bytes / 1048576).toFixed(1) + "/" + (p.total / 1048576).toFixed(1) + " MB)";
}

async function pollApiStatus() {
  try {
    renderApiStatus(await invoke("get_api_status"));
    renderDownloadProgress(await invoke("get_download_progress"));
    if (mode === "local") {
      isFav = await invoke("is_favorite");
      renderFav();
    }
    const rem = await invoke("get_sleep_timer");
    $("sleep-status").textContent = rem ? `Sleep in ${Math.ceil(rem / 60)} min` : "";
  } catch (_) {}
}

/* ---------- queue panel ---------- */

$("btn-queue").onclick = () => {
  const p = $("queue-panel");
  p.classList.toggle("hidden");
  if (!p.classList.contains("hidden")) renderQueuePanel();
};
$("queue-close").onclick = () => $("queue-panel").classList.add("hidden");
$("queue-clear").onclick = () => invoke("queue_clear").then(refresh);

function renderQueuePanel() {
  const el = $("queue-list");
  if (!queueInfo || !queueInfo.items.length) {
    el.innerHTML = '<div class="lyrics-empty">Queue is empty</div>';
    return;
  }
  el.innerHTML = "";
  queueInfo.items.forEach((it, i) => {
    const row = document.createElement("div");
    row.className = "qrow" + (queueInfo.active && i === queueInfo.current ? " active" : "");
    row.innerHTML =
      `<div class="q-meta"><div class="q-title">${escapeHtml(it.title)}</div>` +
      `<div class="q-artist">${escapeHtml(it.artist || "")}</div></div>` +
      `<button class="q-remove" title="Remove">✕</button>`;
    row.onclick = (ev) => {
      if (ev.target.classList.contains("q-remove")) {
        invoke("queue_remove", { index: i }).then(refresh);
        return;
      }
      const items = queueInfo.items;
      invoke("play_queue", { items, start: i }).then(refresh);
    };
    el.appendChild(row);
  });
}

/* ---------- lyrics ---------- */

$("btn-lyrics").onclick = () => {
  const view = $("view-lyrics");
  if (!view.classList.contains("hidden")) {
    showMainView("player");
    return;
  }
  showMainView("lyrics");
  loadLyricsForCurrent();
};

function parseSynced(synced) {
  const lines = [];
  for (const line of synced.split("\n")) {
    const m = line.match(/\[(\d{1,3}):(\d{2}(?:\.\d+)?)\](.*)/);
    if (m) {
      lines.push({
        time: parseInt(m[1], 10) * 60 + parseFloat(m[2]),
        text: m[3].trim(),
      });
    }
  }
  return lines;
}

async function loadLyricsForCurrent() {
  let artist = "";
  let title = "";
  let duration = 0;
  if (queueInfo && queueInfo.active && queueInfo.items[queueInfo.current]) {
    const it = queueInfo.items[queueInfo.current];
    artist = it.artist;
    title = it.title;
    duration = it.duration_secs || (lastState ? lastState.total : 0);
  } else if (lastState && lastState.track) {
    artist = lastState.track.artist;
    title = lastState.track.title;
    duration = lastState.total;
  }
  $("lyrics-title").textContent = title ? `${title} — ${artist || "Unknown"}` : "";
  const body = $("lyrics-body");
  body.innerHTML = '<div class="lyrics-empty">Loading lyrics...</div>';
  if (!title) {
    body.innerHTML = '<div class="lyrics-empty">Nothing is playing.</div>';
    return;
  }
  try {
    const r = await invoke("get_lyrics", { artist, title, durationSecs: duration });
    if (r.synced) {
      lyricsLines = parseSynced(r.synced);
      body.innerHTML = "";
      lyricsLines.forEach((l, i) => {
        const div = document.createElement("div");
        div.className = "lyric-line";
        div.dataset.time = l.time;
        div.textContent = l.text;
        div.onclick = () => invoke("seek", { secs: l.time });
        body.appendChild(div);
      });
    } else if (r.plain) {
      lyricsLines = [];
      body.innerHTML = "";
      r.plain.split("\n").forEach((line) => {
        const div = document.createElement("div");
        div.className = "lyric-line plain";
        div.textContent = line;
        body.appendChild(div);
      });
    } else {
      lyricsLines = [];
      body.innerHTML = `<div class="lyrics-empty">${r.error || "No lyrics found for this track"}</div>`;
    }
  } catch (e) {
    lyricsLines = [];
    body.innerHTML = `<div class="lyrics-empty">Failed to load lyrics: ${escapeHtml(String(e))}</div>`;
  }
  lyricsActiveIdx = -1;
}

function highlightLyrics() {
  const view = $("view-lyrics");
  if (view.classList.contains("hidden") || !lyricsLines.length || !lastState) return;
  const elapsed = lastState.elapsed;
  let idx = -1;
  for (let i = 0; i < lyricsLines.length; i++) {
    if (lyricsLines[i].time <= elapsed) idx = i;
    else break;
  }
  if (idx === lyricsActiveIdx) return;
  lyricsActiveIdx = idx;
  const lines = $("lyrics-body").querySelectorAll(".lyric-line");
  lines.forEach((el, i) => el.classList.toggle("active", i === idx));
  if (idx >= 0 && lines[idx]) {
    lines[idx].scrollIntoView({ block: "center", behavior: "smooth" });
  }
}

/* ---------- playback speed ---------- */

const SPEEDS = [0.75, 1, 1.25, 1.5, 2];
let speedIdx = 1;
$("btn-speed").onclick = () => {
  speedIdx = (speedIdx + 1) % SPEEDS.length;
  const s = SPEEDS[speedIdx];
  invoke("set_speed", { speed: s });
  $("btn-speed").textContent = s + "x";
};

/* ---------- favorites ---------- */

function renderFav() {
  $("btn-fav").textContent = isFav ? "♥" : "♡";
  $("btn-fav").classList.toggle("on", isFav);
}

$("btn-fav").onclick = async () => {
  try {
    isFav = await invoke("toggle_favorite");
    renderFav();
  } catch (e) {
    showError(e);
  }
};

$("btn-favorites").onclick = async () => {
  try {
    hideError();
    const infos = await invoke("open_favorites");
    trackList = infos.map((t) => ({
      title: t.title,
      artist: t.artist,
      duration: t.duration,
    }));
    trackListKey = null;
    const coverPath = (infos.find((t) => t.cover) || {}).cover;
    setListHeader({
      title: "Favorites",
      subtitle: `${infos.length} tracks`,
      cover: coverPath ? convertFileSrc(coverPath) : "",
    });
    refresh();
  } catch (e) {
    showError(e);
  }
};

/* ---------- history / related ---------- */

$("yt-history-btn").onclick = async () => {
  try {
    hideError();
    const h = await invoke("get_history");
    const tracks = h.map((x) => ({
      video_id: x.id,
      title: x.title,
      artist: x.artist,
      duration_secs: 0,
      thumbnail: x.thumbnail,
    }));
    setYtTracks(tracks, false, {
      title: "History",
      subtitle: `${tracks.length} tracks`,
      cover: tracks[0] ? tracks[0].thumbnail || "" : "",
    });
    $("yt-more-btn").classList.add("hidden");
  } catch (e) {
    showError(e);
  }
};

$("lh-related").onclick = async () => {
  if (!queueInfo || !queueInfo.active) return;
  const it = queueInfo.items[queueInfo.current];
  if (!it) return;
  try {
    hideError();
    const tracks = await invoke("recommend_tracks", { videoId: it.id });
    setYtTracks(tracks, false, {
      title: `Related: ${it.title}`,
      subtitle: `${tracks.length} tracks`,
      cover: tracks[0] ? tracks[0].thumbnail || "" : "",
    });
    $("yt-more-btn").classList.add("hidden");
  } catch (e) {
    showError(e);
  }
};

/* ---------- sleep timer ---------- */

$("btn-sleep-start").onclick = async () => {
  const m = Number($("sleep-input").value);
  if (!m || m <= 0) return;
  try {
    await invoke("set_sleep_timer", { minutes: m });
    $("sleep-status").textContent = `Sleep in ${m} min`;
  } catch (e) {
    showError(e);
  }
};

$("btn-sleep-cancel").onclick = async () => {
  try {
    await invoke("set_sleep_timer", { minutes: 0 });
    $("sleep-status").textContent = "";
  } catch (_) {}
};

/* ---------- keyboard shortcuts ---------- */

function changeVolume(delta) {
  if (!lastState) return;
  const v = Math.min(1, Math.max(0, lastState.volume + delta));
  volumeEl.value = Math.round(v * 100);
  $("volume-value").textContent = Math.round(v * 100) + "%";
  invoke("set_volume", { volume: v }).catch(() => {});
}

window.addEventListener("keydown", (e) => {
  const tag = (document.activeElement && document.activeElement.tagName) || "";
  if (tag === "INPUT" || tag === "TEXTAREA") return;
  if (e.code === "Space") {
    e.preventDefault();
    invoke("toggle_pause").then(refresh);
  } else if (e.code === "ArrowLeft") {
    e.preventDefault();
    invoke("seek", { secs: Math.max(0, (lastState ? lastState.elapsed : 0) - 5) });
  } else if (e.code === "ArrowRight") {
    e.preventDefault();
    invoke("seek", { secs: (lastState ? lastState.elapsed : 0) + 5 });
  } else if (e.code === "ArrowUp") {
    e.preventDefault();
    changeVolume(0.05);
  } else if (e.code === "ArrowDown") {
    e.preventDefault();
    changeVolume(-0.05);
  }
  if (e.ctrlKey && e.code === "ArrowRight") {
    e.preventDefault();
    invoke("next_track").then(refresh);
  } else if (e.ctrlKey && e.code === "ArrowLeft") {
    e.preventDefault();
    invoke("prev_track").then(refresh);
  }
});

/* ---------- drag & drop ---------- */

try {
  const { getCurrentWebview } = window.__TAURI__.webview;
  getCurrentWebview().onDragDropEvent((ev) => {
    if (ev.payload.type === "drop") {
      const p = ev.payload.paths && ev.payload.paths[0];
      if (p) {
        invoke("open_folder", { path: p })
          .then((lib) => {
            renderPlaylists(lib);
            setMode("local");
          })
          .catch((e) => showError(e));
      }
    }
  });
} catch (_) {}

refresh();
loadLibrary();
loadSettings();
pollApiStatus();
setInterval(refresh, 250);
setInterval(pollApiStatus, 3000);
setInterval(highlightLyrics, 250);

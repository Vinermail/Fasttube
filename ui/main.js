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
let viewHistory = ["player"];
let viewHistoryIdx = 0;
let currentViewName = "player";

/// Transport noise is not something a listener can act on: say what happened
/// in one line instead of dumping a tonic error.
function playlistError(e) {
  const text = String(e || "");
  if (/Timeout expired|DeadlineExceeded|deadline|unavailable|tcp connect|connection/i.test(text)) {
    return "YouTube не ответил вовремя — попробуйте открыть ещё раз";
  }
  return text.length > 200 ? text.slice(0, 200) + "…" : text;
}

function showError(message) {
  $("sidebar-error").textContent = String(message);
  $("sidebar-error").classList.remove("hidden");
}

function hideError() {
  $("sidebar-error").classList.add("hidden");
}

/* ---------- cheap DOM writes ---------- */
// The status poll used to rewrite the same strings four times a second. These
// helpers remember the last value and touch the DOM only when it differs.

const textCache = new WeakMap();

function setText(el, value) {
  const text = value == null ? "" : String(value);
  if (textCache.get(el) === text) return;
  textCache.set(el, text);
  el.textContent = text;
}

function setClass(el, cls) {
  if (el.className !== cls) el.className = cls;
}

function setValue(el, value) {
  if (el.value !== String(value)) el.value = value;
}

function setSrc(el, src) {
  if (el.getAttribute("src") !== src) {
    if (src) el.setAttribute("src", src);
    else el.removeAttribute("src");
  }
}

/* ---------- where the clock is ---------- */
// Between polls the position is extrapolated from the last snapshot, so the
// progress bar still moves while the app asks the engine only once a second.

let lastStateAt = 0;

function interpolatedElapsed() {
  if (!lastState) return 0;
  const base = lastState.elapsed || 0;
  if (lastState.status !== "playing") return base;
  return base + (performance.now() - lastStateAt) / 1000;
}

// A seek is applied asynchronously, so the next snapshot still carries the old
// position and the bar visibly jumps back for a second. Hold the target until
// the engine agrees with it (or the seek clearly failed).
let seekPending = null;
const SEEK_SETTLE_SECS = 0.75;
const SEEK_SETTLE_MS = 1200;

function applySeek(secs) {
  if (!(secs >= 0)) return;
  if (!lastState) {
    invoke("seek", { secs }).catch(() => {});
    return;
  }
  const total = lastState.total || 0;
  const target = total > 0 ? Math.min(secs, total) : secs;
  seekPending = {
    target,
    at: performance.now(),
    idx: lastState.idx,
    playing: lastState.status === "playing",
  };
  invoke("seek", { secs: target }).catch(() => {});
  // Paint the target at once: waiting for the engine to echo it back is what
  // made the handle flick backwards.
  const max = total > 0 ? total : 100;
  if (seekEl.max !== String(max)) seekEl.max = max;
  setValue(seekEl, target);
  setText($("time-elapsed"), fmt(target));
}

/// The position to draw: the seek target while the engine is catching up,
/// the interpolated snapshot otherwise.
function currentElapsed() {
  if (!lastState) return seekPending ? seekPending.target : 0;
  if (seekPending) {
    const settled =
      lastState.idx === seekPending.idx &&
      Math.abs((lastState.elapsed || 0) - seekPending.target) <= SEEK_SETTLE_SECS;
    const stale = performance.now() - seekPending.at > SEEK_SETTLE_MS;
    if (settled || stale || lastState.idx !== seekPending.idx) {
      seekPending = null;
    } else {
      const drift = seekPending.playing ? (performance.now() - seekPending.at) / 1000 : 0;
      return seekPending.target + drift;
    }
  }
  return interpolatedElapsed();
}

/* ---------- modes ---------- */

function applyMode(m) {
  // There is no Local/YouTube switch any more: the sidebar shows both kinds of
  // playlist at once. The mode only decides whether a queue can be shuffled.
  mode = m;
  const showShuffle = mode === "youtube";
  $("btn-shuffle").style.display = showShuffle ? "" : "none";
  $("lh-shuffle").style.display = showShuffle ? "" : "none";
}

function setMode(m) {
  applyMode(m);
  showMainView("player");
}

function showMainView(name) {
  if (name === currentViewName) {
    applyView(name);
    return;
  }
  // Record navigation (truncate any forward history).
  viewHistory = viewHistory.slice(0, viewHistoryIdx + 1);
  viewHistory.push(name);
  viewHistoryIdx = viewHistory.length - 1;
  if (viewHistory.length > 100) {
    viewHistory.shift();
    viewHistoryIdx--;
  }
  applyView(name);
  updateNavButtons();
}

function applyView(name) {
  currentViewName = name;
  document.querySelectorAll(".view").forEach((v) => v.classList.add("hidden"));
  $("view-" + name).classList.remove("hidden");
  $("mode-settings").classList.toggle("active", name === "settings");
  $("mode-home").classList.toggle("active", name === "home");
  if (name === "lyrics") startLyricsTicker();
  else stopLyricsTicker();
}

function goBack() {
  if (viewHistoryIdx <= 0) return;
  viewHistoryIdx--;
  applyView(viewHistory[viewHistoryIdx]);
  updateNavButtons();
}

function goForward() {
  if (viewHistoryIdx >= viewHistory.length - 1) return;
  viewHistoryIdx++;
  applyView(viewHistory[viewHistoryIdx]);
  updateNavButtons();
}

function updateNavButtons() {
  $("btn-back").classList.toggle("disabled", viewHistoryIdx <= 0);
  $("btn-forward").classList.toggle("disabled", viewHistoryIdx >= viewHistory.length - 1);
}

$("btn-back").onclick = goBack;
$("btn-forward").onclick = goForward;

$("mode-home").onclick = () => {
  showMainView("home");
  if (!homeLoaded) loadHome();
};

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
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
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

// The result is keyed by the list version, the filter text and the sort mode,
// so a poll that changes nothing reuses the previous array: no copies, no
// sorting. Entries keep the row and its original index instead of copying the
// row itself.
let filterSortCache = { key: null, out: [] };

function applyFilterSort(rows, version) {
  const key = version + "|" + filterText + "|" + sortMode + "|" + rows.length;
  if (filterSortCache.key === key) return filterSortCache.out;

  const out = [];
  const q = filterText ? filterText.toLowerCase() : "";
  rows.forEach((row, i) => {
    if (q) {
      const hit =
        (row.title || "").toLowerCase().includes(q) ||
        (row.artist || "").toLowerCase().includes(q);
      if (!hit) return;
    }
    out.push({ row, orig: i });
  });
  if (sortMode === 1) out.sort((a, b) => a.row.title.localeCompare(b.row.title));
  else if (sortMode === 2)
    out.sort((a, b) => (a.row.artist || "").localeCompare(b.row.artist || ""));
  else if (sortMode === 3) out.sort((a, b) => a.row.duration - b.row.duration);

  filterSortCache = { key, out };
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


/* ---------- sidebar: pinned, plain, hidden; plus the user's own lists -------- */

let localPlaylists = [];
let ytPlaylists = [];
let playlistStore = { pinned: [], hidden: [], meta: {}, custom: [] };
let hiddenOpen = false;
let currentLocalPlaylistId = null;
let currentYtPlaylistId = null;
let currentYtPlaylistOwned = false;
// Ownership learned from every page we have seen: editing is offered only for
// playlists the account actually owns.
const knownOwned = new Map();

async function playlistOwned(entry) {
  if (!entry.ytId) return false;
  if (knownOwned.has(entry.ytId)) return knownOwned.get(entry.ytId);
  try {
    const page = await invoke("get_playlist_tracks", {
      playlistId: entry.ytId,
      offset: 0,
      limit: 1,
    });
    knownOwned.set(entry.ytId, !!page.owned);
    return !!page.owned;
  } catch (_) {
    return false;
  }
}

function allPlaylists() {
  const custom = (playlistStore.custom || []).map((c) => ({
    id: c.id,
    name: c.title,
    tracks: (c.tracks || []).length,
    custom: true,
    // Rows of both kinds: the count in the sidebar is what the user sees.
    youtube: (c.tracks || []).filter((t) => t.kind === "youtube").length,
    linkedId: c.linked_id || null,
  }));
  return [...custom, ...localPlaylists, ...ytPlaylists];
}

function playlistTitle(entry) {
  const meta = playlistStore.meta && playlistStore.meta[entry.id];
  return (meta && meta.title) || entry.name || "";
}

function playlistCover(entry) {
  const meta = playlistStore.meta && playlistStore.meta[entry.id];
  if (meta && meta.cover) return convertFileSrc(meta.cover);
  if (entry.thumbnail) return entry.thumbnail;
  return "";
}

async function loadPlaylistStore() {
  try {
    playlistStore = await invoke("get_playlist_store");
  } catch (e) {
    console.warn("could not load the playlist settings:", String(e));
  }
  renderSidebar();
}

function sidebarRow(entry) {
  const div = document.createElement("div");
  const pinned = (playlistStore.pinned || []).includes(entry.id);
  div.className = "item pl-item" + (pinned ? " pinned" : "");
  div.dataset.id = entry.id;

  const cover = playlistCover(entry);
  if (cover) {
    const img = document.createElement("img");
    img.className = "yt-thumb";
    img.loading = "lazy";
    img.alt = "";
    img.src = cover;
    div.appendChild(img);
  }
  if (pinned) {
    const pin = document.createElement("span");
    pin.className = "pin-mark";
    pin.textContent = "📌";
    pin.title = "Закреплён";
    div.appendChild(pin);
  }
  const name = document.createElement("span");
  name.className = "pl-name";
  name.textContent = playlistTitle(entry);
  div.appendChild(name);
  const count = document.createElement("span");
  count.className = "count";
  count.textContent = `(${entry.tracks || 0})`;
  div.appendChild(count);

  div.onclick = () => openSidebarPlaylist(entry);
  div.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    openPlaylistMenu(e.clientX, e.clientY, entry).catch((err) => showError(err));
  });
  return div;
}

function renderSidebar() {
  const el = $("playlists");
  if (!el) return;
  el.innerHTML = "";
  $("yt-user-playlists").innerHTML = "";

  const everything = allPlaylists();
  const byId = new Map(everything.map((p) => [p.id, p]));
  const pinnedIds = (playlistStore.pinned || []).filter((id) => byId.has(id));
  const hiddenIds = (playlistStore.hidden || []).filter((id) => byId.has(id));

  if (pinnedIds.length) {
    const head = document.createElement("div");
    head.className = "pl-group";
    head.textContent = "Закреплённые";
    el.appendChild(head);
    pinnedIds.forEach((id) => el.appendChild(sidebarRow(byId.get(id))));
  }

  const rest = everything.filter(
    (p) => !pinnedIds.includes(p.id) && !hiddenIds.includes(p.id)
  );
  if (pinnedIds.length && rest.length) {
    const head = document.createElement("div");
    head.className = "pl-group";
    head.textContent = "Все плейлисты";
    el.appendChild(head);
  }
  rest.forEach((p) => el.appendChild(sidebarRow(p)));

  if (hiddenIds.length) {
    const folder = document.createElement("div");
    folder.className = "pl-group pl-hidden-toggle";
    folder.textContent = `${hiddenOpen ? "▾" : "▸"} Скрытые (${hiddenIds.length})`;
    folder.onclick = () => {
      hiddenOpen = !hiddenOpen;
      renderSidebar();
    };
    el.appendChild(folder);
    if (hiddenOpen) {
      hiddenIds.forEach((id) => el.appendChild(sidebarRow(byId.get(id))));
    }
  }
}

async function openSidebarPlaylist(entry) {
  try {
    hideError();
    if (entry.custom) {
      if (entry.linkedId) {
        // Linked means YouTube is the source of truth from now on.
        currentLocalPlaylistId = null;
        currentYtPlaylistId = entry.linkedId;
        await loadTrackPages(
          "get_playlist_tracks",
          { playlistId: entry.linkedId },
          (tracks, total) => ({
            title: playlistTitle(entry),
            subtitle: `${total || tracks.length} tracks`,
            cover: playlistCover(entry) || (tracks[0] ? tracks[0].thumbnail || "" : ""),
          })
        );
        $("yt-more-btn").classList.add("hidden");
        return;
      }
      currentLocalPlaylistId = entry.id;
      const infos = await invoke("open_custom_playlist", { id: entry.id });
      applyLocalTrackList(infos, playlistTitle(entry));
      const local = infos.length;
      if (entry.tracks > local) {
        $("lh-subtitle").textContent = `${local} файлов · ${entry.tracks - local} треков YouTube`;
      }
      return;
    }
    if (entry.index !== undefined) {
      currentLocalPlaylistId = entry.id;
      const infos = await invoke("open_playlist", { index: entry.index });
      applyLocalTrackList(infos, playlistTitle(entry));
      return;
    }
    // A YouTube playlist.
    currentLocalPlaylistId = null;
    currentYtPlaylistId = entry.ytId;
    const ytId = entry.ytId;
    await loadTrackPages(
      "get_playlist_tracks",
      { playlistId: ytId },
      (tracks, total) => ({
        title: playlistTitle(entry),
        subtitle: `${total || entry.tracks || tracks.length} tracks`,
        cover: playlistCover(entry) || (tracks[0] ? tracks[0].thumbnail || "" : ""),
      })
    );
    $("yt-more-btn").classList.add("hidden");
  } catch (e) {
    showError(playlistError(e));
  }
}

function applyLocalTrackList(infos, title) {
  currentYtPlaylistOwned = false;
  trackList = infos.map((t) => ({
    title: t.title,
    artist: t.artist,
    duration: t.duration,
    path: t.path,
  }));
  trackListKey = null;
  const coverPath = (infos.find((t) => t.cover) || {}).cover;
  setListHeader({
    title,
    subtitle: `${infos.length} tracks`,
    cover: coverPath ? convertFileSrc(coverPath) : "",
  });
  setMode("local");
  refresh();
}

/* ---------- context menu ---------- */

let ctxMenuEl = null;

function closeContextMenu() {
  if (ctxMenuEl) {
    ctxMenuEl.remove();
    ctxMenuEl = null;
  }
}

function openContextMenu(x, y, items) {
  closeContextMenu();
  if (!items.length) return;
  const menu = document.createElement("div");
  menu.className = "ctx-menu";
  items.forEach((item) => {
    const row = document.createElement("div");
    row.className = "ctx-item" + (item.danger ? " danger" : "");
    row.textContent = item.label;
    row.onclick = (e) => {
      // The document-level click listener would otherwise remove the menu the
      // handler is about to open (a submenu, or the "add to" list).
      e.stopPropagation();
      closeContextMenu();
      Promise.resolve()
        .then(item.run)
        .catch((e) => showError(e));
    };
    menu.appendChild(row);
  });
  document.body.appendChild(menu);
  // Keep it on screen: the menu is placed at the cursor, which may be close to
  // the right or bottom edge.
  const rect = menu.getBoundingClientRect();
  const left = Math.min(x, window.innerWidth - rect.width - 8);
  const top = Math.min(y, window.innerHeight - rect.height - 8);
  menu.style.left = `${Math.max(8, left)}px`;
  menu.style.top = `${Math.max(8, top)}px`;
  ctxMenuEl = menu;
}

document.addEventListener("click", closeContextMenu);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeContextMenu();
});
window.addEventListener("blur", closeContextMenu);

async function openPlaylistMenu(x, y, entry) {
  const pinned = (playlistStore.pinned || []).includes(entry.id);
  const ytId = entry.ytId || null;
  const mix = !!ytId && ytId.startsWith("RD");
  // Saved (not owned) playlists are read-only here; generated mixes never are.
  const owned = ytId && !mix ? await playlistOwned(entry) : false;
  const hidden = (playlistStore.hidden || []).includes(entry.id);
  const items = [
    {
      label: pinned ? "Открепить" : "Закрепить",
      run: async () => {
        await invoke("set_playlist_pinned", { id: entry.id });
        await loadPlaylistStore();
      },
    },
    {
      label: "Переименовать…",
      run: () => askPlaylistTitle(playlistTitle(entry)).then(async (title) => {
        if (title === null) return;
        await invoke("rename_playlist", { id: entry.id, title });
        await loadPlaylistStore();
      }),
    },
    {
      label: "Сменить обложку…",
      run: async () => {
        const file = await dialog.open({
          multiple: false,
          filters: [{ name: "Картинка", extensions: ["jpg", "jpeg", "png", "webp"] }],
        });
        if (!file) return;
        await invoke("set_playlist_cover", { id: entry.id, path: String(file) });
        await loadPlaylistStore();
      },
    },
  ];
  if (entry.custom) {
    items.push({
      label: "Добавить файлы…",
      run: async () => {
        const files = await dialog.open({
          multiple: true,
          filters: [{ name: "Аудио", extensions: ["mp3", "flac", "m4a", "wav", "ogg", "opus"] }],
        });
        const list = Array.isArray(files) ? files : files ? [files] : [];
        if (!list.length) return;
        const added = await invoke("add_tracks_to_playlist", {
          id: entry.id,
          paths: list.map(String),
        });
        await loadPlaylistStore();
        if (!added) showError("эти файлы уже есть в плейлисте");
        else if (currentLocalPlaylistId === entry.id) await openSidebarPlaylist(entry);
      },
    });
  }
  if (entry.custom) {
    if (entry.linkedId) {
      items.push({
        label: "Отвязать от YouTube",
        run: () => unlinkPlaylist(entry),
      });
    } else if (entry.youtube > 0) {
      items.push({
        label: "Связать с YouTube…",
        run: () => linkPlaylistToYoutube(entry),
      });
    }
    items.push({ label: "Экспорт в M3U…", run: () => exportPlaylistM3u(entry) });
  }
  if (ytId && owned) {
    items.push({
      label: "Добавить треки на YouTube…",
      run: () => pickTracksForYtPlaylist(ytId, playlistTitle(entry)),
    });
  }
  items.push({
    label: "Создать плейлист…",
    run: () => askWhereToCreate(),
  });
  items.push({
    label: hidden ? "Показать в списке" : "Скрыть в папку",
    run: async () => {
      await invoke("set_playlist_hidden", { id: entry.id });
      await loadPlaylistStore();
    },
  });
  if (ytId && !owned && !mix) {
    items.push({
      label: "Сохранить в медиатеку",
      run: () => savePlaylistToLibrary(ytId),
    });
    items.push({
      label: "Сделать копию…",
      run: () => copyPlaylistToAccount(ytId, playlistTitle(entry)),
    });
    items.push({
      label: "Убрать из медиатеки…",
      danger: true,
      run: async () => {
        const ok = await askConfirm(
          `Убрать «${playlistTitle(entry)}» из медиатеки? Плейлист останется на YouTube.`
        );
        if (!ok) return;
        await invoke("remove_from_library", { playlistId: ytId });
        knownOwned.delete(ytId);
        if (currentYtPlaylistId === ytId) {
          currentYtPlaylistId = null;
          currentYtTracks = [];
          ytRows = [];
          ytListVersion++;
          refresh();
        }
        await loadUserPlaylists();
        await loadPlaylistStore();
      },
    });
  }
  if (ytId && owned) {
    items.push({
      label: "Удалить на YouTube…",
      danger: true,
      run: async () => {
        const ok = await askConfirm(
          `Удалить «${playlistTitle(entry)}» на YouTube? Это необратимо.`
        );
        if (!ok) return;
        await invoke("delete_account_playlist", { playlistId: ytId });
        if (currentYtPlaylistId === ytId) {
          currentYtPlaylistId = null;
          currentYtTracks = [];
          ytRows = [];
          ytListVersion++;
          refresh();
        }
        await loadUserPlaylists();
        await loadPlaylistStore();
      },
    });
  }
  if (entry.custom) {
    items.push({
      label: "Удалить плейлист",
      danger: true,
      run: async () => {
        if (!(await askConfirm(`Удалить «${playlistTitle(entry)}»?`))) return;
        await invoke("delete_playlist", { id: entry.id });
        if (currentLocalPlaylistId === entry.id) {
          currentLocalPlaylistId = null;
          trackList = [];
          trackListKey = null;
          refresh();
        }
        await loadPlaylistStore();
      },
    });
  }
  openContextMenu(x, y, items);
}

/* ---------- small dialogs (rename, new playlist, confirm) ---------- */

function askPlaylistTitle(initial) {
  return new Promise((resolve) => {
    const wrap = document.createElement("div");
    wrap.className = "modal-backdrop";
    const box = document.createElement("div");
    box.className = "modal-box";
    const label = document.createElement("div");
    label.className = "modal-title";
    label.textContent = "Название плейлиста";
    const input = document.createElement("input");
    input.className = "modal-input";
    input.value = initial || "";
    const hint = document.createElement("p");
    hint.className = "hint";
    hint.textContent = "Пустое имя вернёт название из источника.";
    const row = document.createElement("div");
    row.className = "modal-actions";
    const cancel = document.createElement("button");
    cancel.className = "btn";
    cancel.textContent = "Отмена";
    const ok = document.createElement("button");
    ok.className = "btn primary";
    ok.textContent = "Сохранить";
    row.append(cancel, ok);
    box.append(label, input, hint, row);
    wrap.appendChild(box);
    document.body.appendChild(wrap);
    input.focus();
    input.select();

    const done = (value) => {
      wrap.remove();
      resolve(value);
    };
    cancel.onclick = () => done(null);
    ok.onclick = () => done(input.value);
    input.onkeydown = (e) => {
      if (e.key === "Enter") done(input.value);
      if (e.key === "Escape") done(null);
    };
    wrap.onclick = (e) => {
      if (e.target === wrap) done(null);
    };
  });
}

function askConfirm(text) {
  return new Promise((resolve) => {
    const wrap = document.createElement("div");
    wrap.className = "modal-backdrop";
    const box = document.createElement("div");
    box.className = "modal-box";
    const label = document.createElement("div");
    label.className = "modal-title";
    label.textContent = text;
    const row = document.createElement("div");
    row.className = "modal-actions";
    const cancel = document.createElement("button");
    cancel.className = "btn";
    cancel.textContent = "Отмена";
    const ok = document.createElement("button");
    ok.className = "btn danger";
    ok.textContent = "Удалить";
    row.append(cancel, ok);
    box.append(label, row);
    wrap.appendChild(box);
    document.body.appendChild(wrap);
    const done = (value) => {
      wrap.remove();
      resolve(value);
    };
    cancel.onclick = () => done(false);
    ok.onclick = () => done(true);
    wrap.onclick = (e) => {
      if (e.target === wrap) done(false);
    };
  });
}

async function createCustomPlaylist() {
  const title = await askPlaylistTitle("");
  if (title === null) return;
  const id = await invoke("create_playlist", { title });
  await loadPlaylistStore();
  const entry = allPlaylists().find((p) => p.id === id);
  if (entry) await openSidebarPlaylist(entry);
}

/* ---------- local playlists ---------- */

function renderPlaylists(library) {
  localPlaylists = library.map((pl, i) => ({
    id: pl.id || `local:index:${i}`,
    name: pl.name,
    tracks: pl.tracks,
    index: i,
  }));
  renderSidebar();
}

async function loadLibrary() {
  try {
    renderPlaylists(await invoke("get_library"));
  } catch (e) {
    showError(e);
  }
}

event.listen("library-loaded", () => {
  loadLibrary();
  // The local shelves depend on the library: «Избранное» and «Недавно
  // добавленное» would otherwise stay as they were before the scan.
  homeLastKey = "";
  loadHome(false);
});

/* ---------- youtube lists ---------- */

function playYtAt(i) {
  const items = currentYtTracks.map((t) => ({
    id: t.video_id,
    title: t.title,
    artist: t.artist,
    thumbnail: t.thumbnail || "",
    durationSecs: t.duration_secs,
  }));
  setMode("youtube");
  invoke("play_queue", { items, start: i }).catch((e) => showError(e));
}

// Show the given tracks on the right side (search results, liked songs,
// user playlist contents). Playback is not started.
let ytRows = [];
let ytListVersion = 0;

function setYtTracks(tracks, append, meta) {
  currentYtTracks = append ? currentYtTracks.concat(tracks) : tracks;
  if (!append) ytListVersion++;
  ytRows = currentYtTracks.map((t) => ({
    id: t.video_id,
    title: t.title,
    artist: t.artist,
    duration: t.duration_secs,
    setVideoId: t.set_video_id || "",
  }));
  if (meta) {
    setListHeader(meta);
  }
  if (!append && tracks.length) {
    // Warm the backend: resolve the first few tracks in parallel so the
    // first click starts instantly.
    const items = tracks.slice(0, 4).map((t) => ({
      id: t.video_id,
      title: t.title,
      artist: t.artist,
      thumbnail: t.thumbnail || "",
      durationSecs: t.duration_secs,
    }));
    invoke("prefetch_tracks", { items }).catch(() => {});
  }
  refresh();
}

/// How many rows the backend hands over per request.
const TRACK_PAGE = 100;

/// Appends a page of tracks to the list already on screen. The list is
/// virtualised, so this repaints only the visible window and keeps the scroll
/// position — a page arriving must not throw the user back to the top.
function appendYtTracks(tracks) {
  currentYtTracks = currentYtTracks.concat(tracks);
  ytRows = currentYtTracks.map((t) => ({
    id: t.video_id,
    title: t.title,
    artist: t.artist,
    duration: t.duration_secs,
    setVideoId: t.set_video_id || "",
  }));
  ytListVersion++;
  trackListEl = $("tracks");
  trackRows = applyFilterSort(ytRows, "y:" + ytListVersion);
  renderedTracksKey = "y:" + ytListVersion + ":" + filterText + ":" + sortMode;
  updateTracks();
}

/// Draws the first page at once, then keeps asking for the rest (and appending
/// it) while the user is already reading the top of the list.
async function loadTrackPages(command, args, meta) {
  let offset = 0;
  let total = 0;
  let seen = 0;
  let first = true;
  for (let guard = 0; guard < 200; guard++) {
    const page = await invoke(command, { ...args, offset, limit: TRACK_PAGE });
    if (page.error) throw new Error(page.error);
    const tracks = page.tracks || [];
    if (page.total > 0) total = page.total;
    if (currentYtPlaylistId) {
      currentYtPlaylistOwned = !!page.owned;
      knownOwned.set(currentYtPlaylistId, !!page.owned);
    }
    if (first) {
      setMode("youtube");
      setYtTracks(tracks, false, meta(tracks, total));
      first = false;
    } else if (tracks.length) {
      appendYtTracks(tracks);
    }
    seen += tracks.length;
    offset += tracks.length;
    if (!tracks.length) break;
    if (total > 0 && seen >= total) break;
    if (tracks.length < TRACK_PAGE) break;
  }
  return seen;
}

/* ---------- music search: chips, suggestions, sections ---------- */

const SEARCH_FILTERS = ["", "songs", "videos", "albums", "artists", "playlists"];
let searchFilter = "songs";
// Bumped for every query: a late answer whose seq is stale is thrown away,
// which is what keeps fast typing from rendering older results on top.
let searchSeq = 0;
let suggestSeq = 0;
let suggestTimer = null;
let suggestItems = [];
let suggestIndex = -1;
let lastSearchQuery = "";

function setSearchFilter(filter, persist) {
  searchFilter = SEARCH_FILTERS.includes(filter) ? filter : "songs";
  document.querySelectorAll("#search-chips .chip").forEach((chip) => {
    chip.classList.toggle("active", chip.dataset.filter === searchFilter);
  });
  if (persist) invoke("set_search_filter", { filter: searchFilter }).catch(() => {});
}

$("search-chips").addEventListener("click", (e) => {
  const chip = e.target.closest(".chip");
  if (!chip) return;
  setSearchFilter(chip.dataset.filter, true);
  if (lastSearchQuery) runSearch(lastSearchQuery);
});

function hideSuggest() {
  suggestItems = [];
  suggestIndex = -1;
  $("search-suggest").classList.add("hidden");
}

function renderSuggest() {
  const box = $("search-suggest");
  box.textContent = "";
  if (!suggestItems.length) {
    box.classList.add("hidden");
    return;
  }
  suggestItems.forEach((text, i) => {
    const row = document.createElement("div");
    row.className = "suggest-item" + (i === suggestIndex ? " active" : "");
    row.textContent = text;
    row.onmousedown = (ev) => {
      // mousedown, not click: the input would blur first and hide the list
      ev.preventDefault();
      $("yt-query").value = text;
      hideSuggest();
      runSearch(text);
    };
    box.appendChild(row);
  });
  box.classList.remove("hidden");
}

async function loadSuggestions(query) {
  const seq = ++suggestSeq;
  let items = [];
  try {
    items = await invoke("search_suggestions", { query });
  } catch {
    items = [];
  }
  if (seq !== suggestSeq) return;
  suggestItems = items.slice(0, 8);
  suggestIndex = -1;
  if (document.activeElement === $("yt-query")) renderSuggest();
}

$("yt-query").addEventListener("input", () => {
  const query = $("yt-query").value.trim();
  clearTimeout(suggestTimer);
  suggestSeq++; // any in-flight answer is stale now
  hideSuggest();
  if (query.length < 2) return;
  suggestTimer = setTimeout(() => loadSuggestions(query), 200);
});

function renderSearchStatus(text, retry) {
  const body = $("search-body");
  body.textContent = "";
  body.classList.remove("hidden");
  $("search-empty").classList.add("hidden");
  if (retry) {
    const box = document.createElement("div");
    box.className = "s-error";
    box.textContent = text;
    const again = document.createElement("button");
    again.className = "btn small";
    again.textContent = "Повторить";
    again.onclick = () => runSearch(lastSearchQuery);
    box.appendChild(again);
    body.appendChild(box);
  } else {
    const box = document.createElement("div");
    box.className = "s-status";
    box.textContent = text;
    body.appendChild(box);
  }
}

function renderSearchSkeleton() {
  const body = $("search-body");
  body.textContent = "";
  body.classList.remove("hidden");
  $("search-empty").classList.add("hidden");
  const section = document.createElement("div");
  section.className = "s-section";
  const head = document.createElement("div");
  head.className = "s-section-head";
  head.textContent = "Поиск…";
  section.appendChild(head);
  for (let i = 0; i < 6; i++) {
    const row = document.createElement("div");
    row.className = "s-row skeleton";
    row.innerHTML =
      '<div class="s-art"></div><div class="s-lines"><i></i><i></i></div><div class="s-dur"></div><div></div>';
    section.appendChild(row);
  }
  body.appendChild(section);
}

function thumbSrc(item) {
  if (!item.thumbnail) return "";
  // Local library covers live outside the asset scope as plain paths.
  return item.path ? convertFileSrc(item.thumbnail) : item.thumbnail;
}

function playSectionItem(section, index) {
  const item = section.items[index];
  if (item.path) {
    // Library files play inside their folder; a file that is not part of the
    // library (a cached favourite, for instance) still has to start.
    invoke("play_local_path", { path: item.path })
      .then(() => applyMode("local"))
      .catch(() =>
        invoke("play_local_file", { path: item.path })
          .then(() => applyMode("local"))
          .catch((e) => showError(e)),
      );
    return;
  }
  const playable = section.items.filter(
    (it) => !it.path && (it.kind === "song" || it.kind === "video"),
  );
  const from = playable.findIndex((it) => it.id === item.id);
  if (from < 0) return;
  // The clicked track plus everything after it in this section.
  const items = playable.slice(from).map((it) => ({
    id: it.id,
    title: it.title,
    artist: it.subtitle || "",
    thumbnail: it.thumbnail || "",
    durationSecs: it.duration_secs || 0,
  }));
  applyMode("youtube");
  invoke("play_queue", { items, start: 0 }).catch((e) => showError(e));
}

function queueSearchItem(item, playNext) {
  if (item.path) return; // the YouTube queue cannot hold local files
  const payload = {
    id: item.id,
    title: item.title,
    artist: item.subtitle || "",
    thumbnail: item.thumbnail || "",
    durationSecs: item.duration_secs || 0,
  };
  invoke("queue_add", { items: [payload], playNext })
    .then(() => refresh())
    .catch((e) => showError(e));
}

function buildSearchRow(section, index) {
  const item = section.items[index];
  const row = document.createElement("div");
  row.className = "s-row";

  const art = document.createElement("div");
  art.className = "s-art";
  const src = thumbSrc(item);
  if (src) {
    const img = document.createElement("img");
    img.loading = "lazy";
    img.alt = "";
    img.src = src;
    art.appendChild(img);
  } else {
    art.textContent = "♪";
  }

  const lines = document.createElement("div");
  lines.className = "s-lines";
  const title = document.createElement("div");
  title.className = "s-title";
  title.textContent = item.title;
  const sub = document.createElement("div");
  sub.className = "s-sub";
  sub.textContent = item.subtitle || "";
  lines.append(title, sub);

  const dur = document.createElement("div");
  dur.className = "s-dur";
  dur.textContent = item.duration_secs > 0 ? fmt(item.duration_secs) : "";

  const actions = document.createElement("div");
  actions.className = "s-actions";
  if (!item.path) {
    const addBtn = (glyph, label, handler) => {
      const btn = document.createElement("button");
      btn.className = "s-act";
      btn.textContent = glyph;
      btn.title = label;
      btn.onclick = (ev) => {
        ev.stopPropagation();
        handler();
      };
      actions.appendChild(btn);
    };
    addBtn("▶", "Играть", () => playSectionItem(section, index));
    addBtn("+", "В очередь", () => queueSearchItem(item, false));
    addBtn("⏭", "Следующим", () => queueSearchItem(item, true));
  }

  row.append(art, lines, dur, actions);
  row.onclick = () => playSectionItem(section, index);
  return row;
}

function openSearchPlaylist(item) {
  hideError();
  currentLocalPlaylistId = null;
  currentYtPlaylistId = item.playlist_id || item.id;
  loadTrackPages(
    "get_playlist_tracks",
    { playlistId: item.playlist_id || item.id },
    (tracks, total) => ({
      title: item.title,
      subtitle: `${total || tracks.length} tracks`,
      cover: item.thumbnail || (tracks[0] ? tracks[0].thumbnail || "" : ""),
    })
  ).catch((e) => showError(playlistError(e)));
}

function buildSearchCard(item) {
  const card = document.createElement("div");
  card.className = "s-card";
  const src = thumbSrc(item);
  if (src) {
    const img = document.createElement("img");
    img.className = "s-card-cover";
    img.loading = "lazy";
    img.alt = "";
    img.src = src;
    card.appendChild(img);
  } else {
    const placeholder = document.createElement("div");
    placeholder.className = "s-card-cover";
    card.appendChild(placeholder);
  }
  const title = document.createElement("div");
  title.className = "s-card-title";
  title.textContent = item.title;
  const sub = document.createElement("div");
  sub.className = "s-card-sub";
  sub.textContent = item.subtitle || "";
  card.append(title, sub);

  if (item.kind === "playlist") {
    card.onclick = () => openSearchPlaylist(item);
  } else if (item.kind === "album" && window.openAlbumPage) {
    card.onclick = () => window.openAlbumPage(item);
  } else if (item.kind === "artist" && window.openArtistPage) {
    card.onclick = () => window.openArtistPage(item);
  } else {
    card.classList.add("static");
  }
  return card;
}

function renderSectionsInto(body, sections, error) {
  body.textContent = "";
  body.classList.remove("hidden");

  for (const section of sections) {
    if (!section.items.length) continue;
    const wrap = document.createElement("div");
    wrap.className = "s-section";
    const head = document.createElement("div");
    head.className = "s-section-head";
    head.textContent = section.title || "";
    wrap.appendChild(head);

    const rowsKind = section.kind === "song" || section.kind === "video";
    if (rowsKind) {
      section.items.forEach((_, i) => wrap.appendChild(buildSearchRow(section, i)));
    } else {
      const grid = document.createElement("div");
      grid.className = "s-cards";
      section.items.forEach((item) => grid.appendChild(buildSearchCard(item)));
      wrap.appendChild(grid);
    }
    body.appendChild(wrap);
  }

  if (!body.childElementCount) {
    const empty = document.createElement("div");
    empty.className = "s-status";
    empty.textContent = "Ничего не найдено";
    body.appendChild(empty);
    return;
  }
  if (error) {
    // Partial answer: the sidecar failed, but something is already shown.
    const note = document.createElement("div");
    note.className = "s-status";
    note.textContent = `Часть результатов недоступна: ${error}`;
    body.appendChild(note);
  }
}

function renderSearchSections(sections, error) {
  $("search-empty").classList.add("hidden");
  renderSectionsInto($("search-body"), sections, error);
}

/* ---------- home ---------- */

let homeLoaded = false;
let homeRefreshing = false;
let homeSession = "signed_out";
let homeLastKey = "";

function homeSkeletonShelf() {
  const shelf = document.createElement("div");
  shelf.className = "h-shelf";
  const head = document.createElement("div");
  head.className = "h-shelf-head";
  const title = document.createElement("div");
  title.className = "h-shelf-title";
  title.textContent = "Загрузка…";
  head.appendChild(title);
  shelf.appendChild(head);

  const rail = document.createElement("div");
  rail.className = "h-rail";
  for (let i = 0; i < 6; i++) {
    const card = document.createElement("div");
    card.className = "h-card skeleton";
    const cover = document.createElement("div");
    cover.className = "h-card-cover";
    const line = document.createElement("div");
    line.className = "h-card-title";
    const sub = document.createElement("div");
    sub.className = "h-card-sub";
    card.append(cover, line, sub);
    rail.appendChild(card);
  }
  shelf.appendChild(rail);
  return shelf;
}

/// Waits until the engine is actually playing before seeking: the stream is
/// resolved and downloaded first, so a fixed delay is a guess. The seek also
/// waits for the decoder to settle (a seek during start-up lands nowhere near
/// the target), and is verified once.
async function seekWhenPlaying(secs) {
  if (!(secs > 1)) return;
  for (let i = 0; i < 60; i++) {
    await new Promise((r) => setTimeout(r, 500));
    const state = await invoke("get_state").catch(() => null);
    if (!state || state.status === "stopped") return;
    if (state.status === "playing" && state.elapsed >= 1.5) {
      await invoke("seek", { secs }).catch(() => {});
      await new Promise((r) => setTimeout(r, 1200));
      const after = await invoke("get_state").catch(() => null);
      if (after && Math.abs((after.elapsed || 0) - secs) > 5) {
        await invoke("seek", { secs }).catch(() => {});
      }
      return;
    }
  }
}

function homeCardClick(shelf, index) {
  const item = shelf.items[index];
  if (!item) return;
  if (item.start_secs > 1) seekWhenPlaying(item.start_secs);
  if (item.path || item.kind === "local") {
    playSectionItem(shelf, index);
  } else if (item.kind === "song" || item.kind === "video") {
    playSectionItem(shelf, index);
  } else if (item.kind === "album" && window.openAlbumPage) {
    window.openAlbumPage(item);
  } else if (item.kind === "artist" && window.openArtistPage) {
    window.openArtistPage(item);
  } else if (item.kind === "playlist") {
    openSearchPlaylist(item);
  }
}

function buildHomeCard(shelf, index) {
  const item = shelf.items[index];
  const card = document.createElement("div");
  card.className = "h-card";
  card.tabIndex = 0;
  card.setAttribute("role", "button");

  const src = thumbSrc(item);
  if (src) {
    const img = document.createElement("img");
    img.className = "h-card-cover";
    img.loading = "lazy";
    img.alt = "";
    img.src = src;
    img.onerror = () => {
      const fallback = document.createElement("div");
      fallback.className = "h-card-cover";
      fallback.textContent = "♪";
      img.replaceWith(fallback);
    };
    card.appendChild(img);
  } else {
    const placeholder = document.createElement("div");
    placeholder.className = "h-card-cover";
    placeholder.textContent = "♪";
    card.appendChild(placeholder);
  }

  const title = document.createElement("div");
  title.className = "h-card-title";
  title.textContent = item.title || "";
  const sub = document.createElement("div");
  sub.className = "h-card-sub";
  sub.textContent = item.subtitle || "";
  card.append(title, sub);

  const activate = () => homeCardClick(shelf, index);
  card.onclick = activate;
  card.onkeydown = (e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      activate();
    }
  };
  return card;
}

function buildHomeShelf(shelf, shelfIndex) {
  const wrap = document.createElement("div");
  wrap.className = "h-shelf";

  const head = document.createElement("div");
  head.className = "h-shelf-head";
  const title = document.createElement("div");
  title.className = "h-shelf-title";
  title.textContent = shelf.title || "";
  const nav = document.createElement("div");
  nav.className = "h-shelf-nav";
  const rail = document.createElement("div");
  rail.className = "h-rail";
  rail.dataset.shelf = String(shelfIndex);

  const makeNav = (glyph, dir) => {
    const btn = document.createElement("button");
    btn.className = "h-nav-btn";
    btn.type = "button";
    btn.textContent = glyph;
    btn.tabIndex = -1;
    btn.onclick = (e) => {
      e.stopPropagation();
      rail.scrollBy({ left: dir * Math.max(240, rail.clientWidth * 0.8), behavior: "smooth" });
    };
    return btn;
  };
  nav.append(makeNav("‹", -1), makeNav("›", 1));
  head.append(title, nav);
  wrap.appendChild(head);

  shelf.items.forEach((_, i) => rail.appendChild(buildHomeCard(shelf, i)));
  wrap.appendChild(rail);
  return wrap;
}

function applyHomeSession() {
  const signedOut = homeSession !== "active";
  $("home-signin").classList.toggle("hidden", !signedOut);
  // Personal shelves belong to the session that fetched them.
  if (signedOut) {
    document.querySelectorAll('#home-body .h-shelf[data-personal="1"]').forEach((el) => el.remove());
  }
}

function renderHome(page, pending) {
  const body = $("home-body");
  const key = JSON.stringify([page.shelves || [], pending, homeSession]);
  if (key === homeLastKey) return;
  homeLastKey = key;

  // Keep the reader's place: both the page and every carousel.
  const scrollTop = body.scrollTop;
  const rails = new Map();
  body.querySelectorAll(".h-rail").forEach((rail) => {
    rails.set(rail.dataset.shelf, rail.scrollLeft);
  });

  body.textContent = "";
  const shelves = page.shelves || [];
  shelves.forEach((shelf, i) => {
    const el = buildHomeShelf(shelf, i);
    if (shelf.kind === "personal") el.dataset.personal = "1";
    body.appendChild(el);
  });
  if (pending && !shelves.some((s) => s.kind === "personal" || s.kind === "explore")) {
    body.insertBefore(homeSkeletonShelf(), body.firstChild);
  }
  if (!body.childElementCount) {
    const empty = document.createElement("div");
    empty.className = "s-status";
    empty.textContent = "Пусто. Включите папку с музыкой или войдите в аккаунт.";
    body.appendChild(empty);
  }

  body.scrollTop = scrollTop;
  rails.forEach((left, index) => {
    const rail = body.querySelector(`.h-rail[data-shelf="${index}"]`);
    if (rail && left > 0) rail.scrollLeft = left;
  });
  applyHomeSession();
  homeLoaded = true;
}

function showHomeStatus(text) {
  $("home-status-text").textContent = text;
  $("home-status").classList.remove("hidden");
}

function hideHomeStatus() {
  $("home-status").classList.add("hidden");
}

async function refreshHome() {
  if (homeRefreshing) return;
  homeRefreshing = true;
  try {
    const page = await invoke("refresh_home");
    hideHomeStatus();
    renderHome(page, false);
  } catch (e) {
    // The snapshot and the local shelves stay on screen: only the network part
    // failed, so there is nothing to replace them with. The placeholder shelf
    // must not keep pretending something is loading.
    showHomeStatus("Не удалось обновить");
    document
      .querySelectorAll("#home-body .h-card.skeleton")
      .forEach((card) => card.closest(".h-shelf")?.remove());
    console.warn("home refresh failed:", String(e));
  } finally {
    homeRefreshing = false;
  }
}

/// `withNetwork = false` re-reads only the local shelves (a library change does
/// not need a round trip to YouTube).
async function loadHome(withNetwork = true) {
  try {
    const status = await invoke("get_api_status");
    homeSession = status.session_state || "signed_out";
    const cached = await invoke("get_home_cached");
    renderHome(cached, withNetwork);
  } catch (e) {
    showError(e);
  }
  if (!withNetwork) return;
  hideHomeStatus();
  refreshHome();
}

$("home-signin-btn").onclick = () => openYtModal();
$("home-retry").onclick = () => {
  hideHomeStatus();
  refreshHome();
};

event.listen("home-updated", (e) => {
  if (e.payload) renderHome(e.payload, false);
});

/* ---------- album / artist pages ---------- */

let browseSections = [];

function albumQueueItems() {
  const section = browseSections.find((s) => s.kind === "song" && s.items.length);
  if (!section) return [];
  return section.items.map((it) => ({
    id: it.id,
    title: it.title,
    artist: it.subtitle || "",
    thumbnail: it.thumbnail || "",
    durationSecs: it.duration_secs || 0,
  }));
}

function playAlbum(shuffle) {
  const items = albumQueueItems();
  if (!items.length) return;
  if (shuffle) {
    for (let i = items.length - 1; i > 0; i--) {
      const j = Math.floor(Math.random() * (i + 1));
      [items[i], items[j]] = [items[j], items[i]];
    }
  }
  applyMode("youtube");
  invoke("play_queue", { items, start: 0 }).catch((e) => showError(e));
}

$("browse-play").onclick = () => playAlbum(false);
$("browse-shuffle").onclick = () => playAlbum(true);

function showBrowsePage(page, isAlbum) {
  $("browse-title").textContent = page.title || "";
  $("browse-subtitle").textContent = page.subtitle || "";
  const cover = $("browse-cover");
  if (page.cover) {
    cover.src = page.cover;
    cover.classList.remove("hidden");
  } else {
    cover.removeAttribute("src");
    cover.classList.add("hidden");
  }
  $("browse-play").classList.toggle("hidden", !isAlbum);
  $("browse-shuffle").classList.toggle("hidden", !isAlbum);
  browseSections = page.sections || [];
  renderSectionsInto($("browse-body"), browseSections, page.error);
  showMainView("browse");
}

async function openBrowsePage(command, item, isAlbum) {
  const browseId = item.browse_id || item.id;
  if (!browseId) return;
  $("browse-title").textContent = item.title || "";
  $("browse-subtitle").textContent = "Загрузка…";
  browseSections = [];
  $("browse-body").textContent = "";
  showMainView("browse");
  try {
    const page = await invoke(command, { browseId });
    showBrowsePage(page, isAlbum);
  } catch (e) {
    $("browse-subtitle").textContent = "";
    const box = document.createElement("div");
    box.className = "s-error";
    box.textContent = String(e);
    const again = document.createElement("button");
    again.className = "btn small";
    again.textContent = "Повторить";
    again.onclick = () => openBrowsePage(command, item, isAlbum);
    box.appendChild(again);
    $("browse-body").textContent = "";
    $("browse-body").appendChild(box);
  }
}

// Stage 3's search cards call these once the pages exist.
window.openAlbumPage = (item) => openBrowsePage("open_album", item, true);
window.openArtistPage = (item) => openBrowsePage("open_artist", item, false);

async function runSearch(query) {
  const seq = ++searchSeq;
  lastSearchQuery = query;
  hideSuggest();
  showMainView("search");
  renderSearchSkeleton();
  let payload;
  try {
    payload = await invoke("search_music", { query, filter: searchFilter });
  } catch (e) {
    if (seq !== searchSeq) return;
    renderSearchStatus(String(e), true);
    return;
  }
  if (seq !== searchSeq) return;
  if (payload.error && !payload.sections.length) {
    renderSearchStatus(payload.error, true);
    return;
  }
  renderSearchSections(payload.sections, payload.error);
}

$("yt-query").addEventListener("focus", () => showMainView("search"));
$("yt-query").addEventListener("blur", () => setTimeout(hideSuggest, 120));

$("yt-query").addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    if (!suggestItems.length) return;
    e.preventDefault();
    const delta = e.key === "ArrowDown" ? 1 : -1;
    suggestIndex = (suggestIndex + delta + suggestItems.length) % suggestItems.length;
    renderSuggest();
    return;
  }
  if (e.key === "Escape") {
    if (suggestItems.length) hideSuggest();
    else $("yt-query").blur();
    return;
  }
  if (e.key === "Enter") {
    e.preventDefault();
    const query = suggestIndex >= 0 ? suggestItems[suggestIndex] : $("yt-query").value.trim();
    if (!query) return;
    $("yt-query").value = query;
    hideSuggest();
    runSearch(query);
  }
});

function shuffleYt() {
  if (!currentYtTracks.length) return;
  // Shuffle only the queue: the main list keeps its original order, the
  // actual (shuffled) order is visible in the Queue panel.
  const arr = currentYtTracks.slice();
  for (let i = arr.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [arr[i], arr[j]] = [arr[j], arr[i]];
  }
  const items = arr.map((t) => ({
    id: t.video_id,
    title: t.title,
    artist: t.artist,
    thumbnail: t.thumbnail || "",
    durationSecs: t.duration_secs,
  }));
  applyMode("youtube");
  invoke("play_queue", { items, start: 0 }).catch((e) => showError(e));
}

$("btn-shuffle").onclick = shuffleYt;

async function renderYtTrackList(invokeName) {
  try {
    hideError();
    if (invokeName === "get_liked_songs") {
      await loadTrackPages("get_liked_songs", {}, (tracks, total) => ({
        title: "Liked songs",
        subtitle: `${total || tracks.length} tracks`,
        cover: tracks[0] ? tracks[0].thumbnail || "" : "",
      }));
      $("yt-more-btn").classList.add("hidden");
      return;
    }
    const tracks = await invoke(invokeName);
    // The list is a YouTube one: without the mode the player keeps painting the
    // local rows and the header would lie about what is on screen.
    setMode("youtube");
    setYtTracks(tracks, false, {
      title: invokeName === "get_liked_songs" ? "Liked songs" : "Tracks",
      subtitle: `${tracks.length} tracks`,
      cover: tracks[0] ? tracks[0].thumbnail || "" : "",
    });
    $("yt-more-btn").classList.add("hidden");
  } catch (e) {
    showError(playlistError(e));
  }
}

$("yt-liked-btn").onclick = () => renderYtTrackList("get_liked_songs");

let ytPlaylistsLoaded = false;

/// The account's playlists are part of the sidebar from the start: they sit in
/// the same list as the local folders, separated by a hairline.
async function loadUserPlaylists() {
  const el = $("yt-user-playlists");
  el.innerHTML = "";
  try {
    const status = await invoke("get_api_status");
    if ((status.session_state || "signed_out") !== "active") {
      ytPlaylists = [];
      ytPlaylistsLoaded = false;
      renderSidebar();
      return;
    }
    const playlists = await invoke("get_user_playlists");
    ytPlaylistsLoaded = true;
    ytPlaylists = playlists.map((p) => ({
      id: `yt:${p.id}`,
      ytId: p.id,
      name: p.name,
      tracks: p.track_count,
      thumbnail: p.thumbnail || "",
    }));
    renderSidebar();
  } catch (e) {
    // No session (or the sidecar is down): the local folders still work, so a
    // banner would be noise.
    ytPlaylistsLoaded = false;
    ytPlaylists = [];
    el.innerHTML = "";
    renderSidebar();
    console.warn("could not load the account playlists:", String(e));
  }
}

/* ---------- auth ---------- */

function setAuthStatus(text, ok) {
  const el = $("yt-auth-status");
  el.textContent = text;
  el.className = "auth-status" + (ok === true ? " ok" : ok === false ? " no" : "");
}

/* ---------- sign-in modal: browser for the login, headers to connect ---------- */

let ytModalReturnFocus = null;

function openYtModal() {
  ytModalReturnFocus = document.activeElement;
  $("yt-modal-error").classList.add("hidden");
  $("yt-modal").classList.remove("hidden");
  // The paste field lives inside a closed <details>, so the primary action
  // gets the focus instead.
  $("yt-modal-window").focus();
}

function closeYtModal() {
  $("yt-modal").classList.add("hidden");
  // The pasted headers are a secret: never leave them in the DOM.
  $("yt-modal-paste").value = "";
  $("yt-modal-error").classList.add("hidden");
  ytModalSetWaiting(false);
  if (ytModalReturnFocus && ytModalReturnFocus.focus) ytModalReturnFocus.focus();
  ytModalReturnFocus = null;
}

/** While the capture window is open, the modal shows what is happening. */
function ytModalSetWaiting(waiting) {
  $("yt-modal-waiting").classList.toggle("hidden", !waiting);
  $("yt-modal-window").disabled = waiting;
  $("yt-modal-connect").disabled = waiting;
  $("yt-modal-open").disabled = waiting;
}

function ytModalIsOpen() {
  return !$("yt-modal").classList.contains("hidden");
}

$("yt-modal-window").onclick = async () => {
  try {
    ytModalError("");
    $("yt-modal-error").classList.add("hidden");
    await invoke("yt_login_start");
    ytModalSetWaiting(true);
  } catch (e) {
    ytModalError(e);
    ytModalSetWaiting(false);
  }
};

function ytModalError(message) {
  const box = $("yt-modal-error");
  if (message === "") {
    box.textContent = "";
    box.classList.add("hidden");
    return;
  }
  box.textContent = String(message || "Something went wrong");
  box.classList.remove("hidden");
}

$("yt-signin-btn").onclick = () => {
  hideError();
  openYtModal();
};

$("yt-modal-close").onclick = () => closeYtModal();

$("yt-modal").addEventListener("click", (e) => {
  // Click on the dimmed backdrop (not inside the card) closes the dialog.
  if (e.target === $("yt-modal")) closeYtModal();
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !$("yt-modal").classList.contains("hidden")) {
    closeYtModal();
  }
});

$("yt-modal-open").onclick = async () => {
  try {
    await invoke("yt_open_login_page");
  } catch (e) {
    ytModalError(e);
  }
};

$("yt-modal-connect").onclick = async () => {
  const text = $("yt-modal-paste").value.trim();
  if (!text) {
    ytModalError("Paste the request headers first");
    return;
  }
  try {
    const info = await invoke("yt_login_advanced", { authText: text });
    // Drop the secret from the field before anything else can touch it.
    $("yt-modal-paste").value = "";
    closeYtModal();
    applySessionInfo(info);
    refresh();
  } catch (e) {
    // The sidecar's rejection is the useful part, so show it inside the modal.
    $("yt-modal-paste").value = "";
    ytModalError(e);
  }
};

function applySessionInfo(info) {
  if (!info) return;
  if (info.state === "active") {
    setAuthStatus(info.account_name ? "Signed in as " + info.account_name : "Signed in", true);
    authBannerDismissed = false;
    // Only the expiry banner belongs to the session; a one-off notice must stay
    // readable until dismissed.
    if (authBannerKind === "expired") hideAuthBanner();
    $("yt-logout-note").classList.add("hidden");
  } else if (info.state === "expired") {
    setAuthStatus("Session expired", false);
  } else {
    setAuthStatus("Not signed in", false);
  }
}

const AUTH_EXPIRED_TEXT =
  "Your YouTube session expired. Sign in again to load your library.";
let authBannerDismissed = false;
let authBannerKind = "";

function showAuthBanner(text, withLogin, kind) {
  $("auth-banner-text").textContent = text;
  $("auth-banner-login").classList.toggle("hidden", !withLogin);
  $("auth-banner").classList.remove("hidden");
  authBannerKind = kind || "";
}

function hideAuthBanner() {
  $("auth-banner").classList.add("hidden");
  authBannerKind = "";
}

$("auth-banner-login").onclick = () => {
  openYtModal();
};

$("auth-banner-close").onclick = () => {
  if (authBannerKind === "expired") authBannerDismissed = true;
  hideAuthBanner();
};

event.listen("auth-expired", () => {
  if (!authBannerDismissed) showAuthBanner(AUTH_EXPIRED_TEXT, true, "expired");
  // Personal shelves must disappear with the session.
  homeSession = "signed_out";
  homeLastKey = "";
  ytPlaylistsLoaded = false;
  $("yt-user-playlists").innerHTML = "";
  loadHome();
});

event.listen("auth-notice", (e) => {
  showAuthBanner(String(e.payload || ""), false, "notice");
});

// The sign-in window reports here; the payload never contains cookies.
event.listen("yt-auth-changed", (e) => {
  const p = e.payload || {};
  ytModalSetWaiting(false);
  if (p.ok) {
    hideError();
    applySessionInfo({ state: p.state || "active", account_name: p.account_name || "" });
    refresh();
    homeSession = p.state || "active";
    homeLastKey = "";
    ytPlaylistsLoaded = false;
    loadUserPlaylists();
    loadHome();
    if (ytModalIsOpen()) closeYtModal();
  } else if (p.cancelled) {
    refresh(); // the user closed the window: status comes from shared state
  } else if (ytModalIsOpen()) {
    // Show the refusal where the user started the flow.
    ytModalError(p.error || "sign-in failed");
    refresh();
  } else {
    showError(p.error || "sign-in failed");
    refresh();
  }
});

$("yt-logout-btn").onclick = async () => {
  try {
    await invoke("logout");
    hideError();
    setAuthStatus("Not signed in", false);
    hideAuthBanner();
    $("yt-logout-note").classList.remove("hidden");
    currentYtTracks = [];
    $("yt-user-playlists").innerHTML = "";
    ytPlaylistsLoaded = false;
    homeSession = "signed_out";
    homeLastKey = "";
    loadHome();
  } catch (e) {
    showError(e);
  }
};

/* ---------- audio quality ---------- */

async function loadSettings() {
  try {
    const s = await invoke("get_settings");
    applyDeveloperMode(s.developer_mode);
    loadPlaylistStore();
    const radio = document.querySelector(`input[name="quality"][value="${s.audio_quality}"]`);
    if (radio) radio.checked = true;
    const norm = document.querySelector(`input[name="normlevel"][value="${s.normalize_level || "medium"}"]`);
    if (norm) norm.checked = true;
    const curve = $("opt-volume-curve");
    if (curve) curve.checked = !!s.volume_curve;
    const streaming = $("opt-streaming-account");
    if (streaming) streaming.checked = !!s.streaming_account;
    const limit = $("opt-cache-limit");
    if (limit) limit.value = String(s.cache_limit_mb ?? 2048);
    setSearchFilter(s.search_filter || "songs", false);
  } catch (e) {
    showError(e);
  }
}

$("opt-cache-limit").addEventListener("change", (e) => {
  invoke("set_cache_limit", { limitMb: Number(e.target.value) }).catch((err) =>
    showError(err),
  );
});

$("opt-streaming-account").addEventListener("change", (e) => {
  invoke("set_streaming_account", { enabled: e.target.checked }).catch((err) => showError(err));
});

$("opt-volume-curve").addEventListener("change", (e) => {
  invoke("set_volume_curve", { enabled: e.target.checked }).catch((err) => showError(err));
});

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
    homeLastKey = "";
    loadHome(false);
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
  seekDragging = false;
  applySeek(seekDragValue);
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

/* ---------- developer mode ---------- */

let devMode = false;

// The native WebView menu (Back, Reload, Save as, Inspect) is noise in a music
// player and lets anyone peek at the internals: it stays off unless the user
// turns the developer mode on.
document.addEventListener(
  "contextmenu",
  (e) => {
    if (!devMode) e.preventDefault();
  },
  true
);

function applyDeveloperMode(enabled) {
  devMode = !!enabled;
  const box = $("opt-dev-mode");
  if (box) box.checked = devMode;
}

window.addEventListener("keydown", (e) => {
  if (!devMode) return;
  if (e.code === "F12" || (e.ctrlKey && e.shiftKey && e.code === "KeyI")) {
    e.preventDefault();
    invoke("open_devtools").catch(() => {});
  }
});

/* ---------- proxy ---------- */

const proxyModeEl = $("opt-proxy-mode");
const proxyKindEl = $("opt-proxy-kind");
const proxyHostEl = $("opt-proxy-host");
const proxyPortEl = $("opt-proxy-port");
let proxyInfo = null;

function renderProxy(status) {
  if (!status) return;
  proxyInfo = status;
  proxyModeEl.value = status.mode || "system";
  proxyKindEl.value = status.kind || "socks5";
  proxyHostEl.value = status.host || "127.0.0.1";
  proxyPortEl.value = String(status.port || 10808);
  const manual = proxyModeEl.value === "manual";
  proxyKindEl.disabled = !manual;
  proxyHostEl.disabled = !manual;
  proxyPortEl.disabled = !manual;
  const where = status.url
    ? (status.source === "manual" ? "сейчас: " : "сейчас (системный): ") + status.url
    : "сейчас: без прокси";
  $("proxy-status").textContent = status.warning ? `${where} · ${status.warning}` : where;
}

async function loadProxy() {
  try {
    renderProxy(await invoke("get_proxy_status"));
  } catch (e) {
    showError(e);
  }
}

proxyModeEl.addEventListener("change", () => {
  renderProxy(Object.assign({}, proxyInfo, { mode: proxyModeEl.value }));
});

$("opt-dev-mode").addEventListener("change", async (e) => {
  const on = e.target.checked;
  applyDeveloperMode(on);
  try {
    await invoke("set_developer_mode", { enabled: on });
  } catch (err) {
    showError(err);
  }
});

$("btn-proxy-apply").onclick = async () => {
  const port = Number(proxyPortEl.value);
  $("proxy-status").textContent = "применяю…";
  try {
    const status = await invoke("set_proxy_settings", {
      mode: proxyModeEl.value,
      kind: proxyKindEl.value,
      host: proxyHostEl.value,
      port: Number.isFinite(port) ? port : 0,
    });
    renderProxy(status);
    hideError();
  } catch (e) {
    $("proxy-status").textContent = String(e);
    showError(e);
  }
};

$("btn-proxy-test").onclick = async () => {
  $("proxy-status").textContent = "проверяю…";
  try {
    const res = await invoke("test_proxy");
    $("proxy-status").textContent = res.detail;
  } catch (e) {
    $("proxy-status").textContent = String(e);
  }
};

/* ---------- mute ---------- */

const muteEl = $("mute-btn");
let muteRendered = null;

function renderMute(muted) {
  const on = !!muted;
  setText(muteEl, on ? "🔇" : "🔊");
  muteEl.classList.toggle("muted", on);
  muteEl.title = on ? "Включить звук (M)" : "Заглушить (M)";
  muteEl.setAttribute("aria-pressed", on ? "true" : "false");
  const box = muteEl.closest(".pb-vol");
  if (box) box.classList.toggle("is-muted", on);
  // The slider keeps its position while muted: dim it, do not move it.
  muteRendered = on;
}

function toggleMute() {
  return invoke("toggle_mute")
    .then((muted) => {
      renderMute(muted);
      if (lastState) lastState.muted = muted;
      refresh();
    })
    .catch((e) => showError(e));
}

muteEl.onclick = () => toggleMute();

// Before anything plays there is no engine snapshot, so the saved level and
// the switch come from settings — otherwise the app would show 80% and
// "unmuted" until the first track starts.
invoke("get_settings")
  .then((s) => {
    const v = typeof s.volume === "number" ? s.volume : 0.8;
    if (!volumeDragging) {
      volumeEl.value = String(Math.round(v * 100));
      $("volume-value").textContent = Math.round(v * 100) + "%";
    }
    renderMute(!!s.muted);
  })
  .catch(() => {});

/* ---------- main rendering ---------- */

const ROW_H = 46;
const OVERSCAN = 8;
let renderedTracksKey = null;
let trackRows = [];
let trackActive = -1;
let lastFirst = -1;
let trackListEl = null;

function buildRow(entry, i) {
  const row = entry.row;
  const div = document.createElement("div");
  const isActive = entry.orig === trackActive;
  div.className = "trow" + (isActive ? " active" : "");
  const dur = row.duration > 0 ? fmt(row.duration) : "";
  div.innerHTML =
    (isActive
      ? `<div class="t-idx"><span class="eq"><i></i><i></i><i></i></span></div>`
      : `<div class="t-idx">${i + 1}</div>`) +
    `<div class="t-title">${escapeHtml(row.title)}</div>` +
    `<div class="t-artist">${escapeHtml(row.artist || "")}</div>` +
    `<div class="t-dur">${dur}</div>`;
  div.addEventListener("contextmenu", (e) => {
    const items = [];
    if (row.path) {
      items.push({
        label: "Добавить в плейлист…",
        run: () => addTrackToPlaylist(row.path),
      });
    } else if (row.id) {
      items.push({
        label: "Добавить в плейлист YouTube…",
        run: () => addVideoToYtPlaylist(row.id, row.title),
      });
    }
    if (currentYtPlaylistId && currentYtPlaylistOwned && !row.setVideoId) {
      // A page cached before the rows carried their per-item id: one refetch
      // makes removal possible again.
      items.push({
        label: "Обновить список (нет id строк)",
        run: async () => {
          await invoke("refresh_playlist_tracks", { playlistId: currentYtPlaylistId });
          await reloadYtPlaylist();
        },
      });
    }
    if (currentYtPlaylistId && currentYtPlaylistOwned && row.setVideoId) {
      items.push({
        label: "Убрать из плейлиста",
        danger: true,
        run: async () => {
          await invoke("remove_playlist_tracks", {
            playlistId: currentYtPlaylistId,
            items: [{ videoId: row.id, setVideoId: row.setVideoId }],
          });
          await reloadYtPlaylist();
        },
      });
    }
    if (currentLocalPlaylistId && String(currentLocalPlaylistId).startsWith("custom:")) {
      items.push({
        label: "Убрать из плейлиста",
        danger: true,
        run: async () => {
          await invoke("remove_track_from_playlist", {
            id: currentLocalPlaylistId,
            index: entry.orig,
          });
          const infos = await invoke("open_custom_playlist", { id: currentLocalPlaylistId });
          applyLocalTrackList(infos, $("list-title") ? $("list-title").textContent : "");
          await loadPlaylistStore();
        },
      });
    }
    if (!items.length) return;
    e.preventDefault();
    e.stopPropagation();
    openContextMenu(e.clientX, e.clientY, items);
  });
  div.onclick = () => {
    const orig = entry.orig;
    if (mode === "youtube") {
      playYtAt(orig);
    } else {
      invoke("play_track", { index: orig }).then(refresh);
    }
  };
  return div;
}

function updateTracks() {
  const el = trackListEl;
  if (!el) return;
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

["tracks"].forEach((id) => {
  const el = document.getElementById(id);
  if (el) {
    el.addEventListener("scroll", () => {
      if (trackListEl !== el) return;
      const first = Math.floor(el.scrollTop / ROW_H);
      if (Math.abs(first - lastFirst) >= OVERSCAN / 2) updateTracks();
    });
  }
});

// Virtualized track list: only the visible rows exist in the DOM, so huge
// playlists stay cheap regardless of their size.
function renderTracks(el, rows, activeIndex, key) {
  trackListEl = el;
  if (key !== renderedTracksKey) {
    renderedTracksKey = key;
    trackRows = rows;
    trackActive = activeIndex;
    el.scrollTop = 0;
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
    renderTracks($("tracks"), applyFilterSort(trackList, key), activeIndex, key + ":" + filterText + ":" + sortMode);
    return;
  }
  try {
    const list = await invoke("get_track_list");
    trackList = list.map((t) => ({
      title: t.title,
      artist: t.artist,
      duration: t.duration,
      path: t.path,
    }));
    trackListKey = key;
    renderTracks($("tracks"), applyFilterSort(trackList, key), activeIndex, key + ":" + filterText + ":" + sortMode);
  } catch (e) {
    showError(e);
  }
}

function renderState(state) {
  lastState = state;
  lastStateAt = performance.now();
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

  setText($("np-title"), npTitle);
  setText($("np-artist"), npArtist);

  const status = state ? state.status : "stopped";
  const statusEl = $("np-status");
  setText(statusEl, status);
  setClass(statusEl, "np-status" + (status !== "stopped" ? " " + status : ""));

  setText($("btn-play"), status === "playing" ? "⏸" : "▶");

  if (state && state.speed) {
    const idx = SPEEDS.indexOf(state.speed);
    if (idx >= 0) speedIdx = idx;
    setText($("btn-speed"), state.speed + "x");
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
  const elapsed = state ? currentElapsed() : 0;
  setText($("time-total"), fmt(total));
  if (!seekDragging) {
    const max = total > 0 ? total : 100;
    if (seekEl.max !== String(max)) seekEl.max = max;
    setValue(seekEl, elapsed);
    setText($("time-elapsed"), fmt(elapsed));
  }

  if (state && !volumeDragging) {
    const pct = Math.round(state.volume * 100);
    setValue(volumeEl, pct);
    setText($("volume-value"), pct + "%");
  }
  if (state && !!state.muted !== muteRendered) {
    renderMute(state.muted);
  }

  if (mode === "youtube") {
    // The main list always shows the opened list in its original order.
    // The playing track is highlighted by id; the shuffled queue order is
    // visible in the Queue panel.
    const queueActive = queueInfo && queueInfo.active && queueInfo.items.length > 0;
    let activeIdx = -1;
    if (queueActive && queueInfo.items[queueInfo.current]) {
      const playingId = queueInfo.items[queueInfo.current].id;
      activeIdx = currentYtTracks.findIndex((t) => t.video_id === playingId);
    }
    $("lh-related").classList.toggle("hidden", !queueActive);
    renderTracks(
      $("tracks"),
      applyFilterSort(ytRows, "y:" + ytListVersion),
      activeIdx,
      "y:" + ytListVersion + ":" + filterText + ":" + sortMode
    );
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

// Polling is adaptive: once a second while a track plays, once every two
// seconds otherwise, and not at all while the window is hidden - the progress
// bar keeps moving from the snapshot it already has.
const POLL_PLAYING_MS = 1000;
const POLL_IDLE_MS = 2000;

let pollTimer = null;

function pollDelay() {
  return lastState && lastState.status === "playing" ? POLL_PLAYING_MS : POLL_IDLE_MS;
}

function schedulePoll(delay) {
  if (pollTimer) clearTimeout(pollTimer);
  pollTimer = null;
  if (document.hidden) return;
  pollTimer = setTimeout(async () => {
    pollTimer = null;
    await refresh();
    schedulePoll();
  }, delay === undefined ? pollDelay() : delay);
}

async function refresh() {
  try {
    await refreshQueue();
    renderState(await invoke("get_state"));
  } catch (e) {
    showError(e);
  }
}

document.addEventListener("visibilitychange", () => {
  if (document.hidden) {
    if (pollTimer) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
    return;
  }
  refresh();
  pollApiStatus();
  schedulePoll();
});

// Local interpolation between polls: only the progress bar and the clock move,
// no IPC, no layout of the list.
function tickProgress() {
  if (document.hidden || seekDragging || !lastState) return;
  if (lastState.status !== "playing") return;
  const total = lastState.total || 0;
  if (!(total > 0)) return;
  const now = currentElapsed();
  setValue(seekEl, Math.min(total, now));
  setText($("time-elapsed"), fmt(now));
}

function renderApiStatus(s) {
  if (!s) return;
  const text = "API: " + (s.state === "ok" ? "ok v" + s.version : s.state);
  const cls = s.state === "ok" ? "ok" : s.state === "down" ? "down" : "starting";
  const el = $("api-badge-side");
  el.textContent = text;
  el.className = "badge api-side " + cls;

  if (s.state === "ok") {
    applySessionInfo({
      state: s.session_state || "signed_out",
      account_name: s.account_name || "",
    });
    if (s.session_state === "expired" && !authBannerDismissed) {
      showAuthBanner(AUTH_EXPIRED_TEXT, true, "expired");
    }
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
  if (document.hidden) return;
  try {
    const status = await invoke("get_api_status");
    renderApiStatus(status);
    // The sidecar finishes signing in after the window is up: the home page
    // has to follow, or a signed-in listener keeps seeing the guest banner.
    const session = status.session_state || "signed_out";
    if (session === "active" && !ytPlaylistsLoaded) loadUserPlaylists();
    if (homeLoaded && session !== homeSession) {
      homeSession = session;
      homeLastKey = "";
      loadHome();
    }
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
        div.onclick = () => applySeek(l.time);
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
      body.innerHTML = `<div class="lyrics-empty">${escapeHtml(r.error || "No lyrics found for this track")}</div>`;
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
  const elapsed = interpolatedElapsed();
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

// The lyrics ticker only exists while the lyrics view is on screen.
let lyricsTicker = null;

function startLyricsTicker() {
  if (lyricsTicker) return;
  highlightLyrics();
  lyricsTicker = setInterval(highlightLyrics, 250);
}

function stopLyricsTicker() {
  if (lyricsTicker) {
    clearInterval(lyricsTicker);
    lyricsTicker = null;
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
    setMode("local");
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
  lastState.muted = false;
  volumeEl.value = Math.round(v * 100);
  $("volume-value").textContent = Math.round(v * 100) + "%";
  invoke("set_volume", { volume: v }).catch(() => {});
}

window.addEventListener("keydown", (e) => {
  if (e.altKey && e.code === "ArrowLeft") {
    e.preventDefault();
    goBack();
    return;
  }
  if (e.altKey && e.code === "ArrowRight") {
    e.preventDefault();
    goForward();
    return;
  }
  const tag = (document.activeElement && document.activeElement.tagName) || "";
  if (tag === "INPUT" || tag === "TEXTAREA") return;
  if (e.code === "KeyM" && !e.ctrlKey && !e.metaKey && !e.altKey) {
    e.preventDefault();
    toggleMute();
    return;
  }
  if (e.code === "Space") {
    e.preventDefault();
    invoke("toggle_pause").then(refresh);
  } else if (e.code === "ArrowLeft") {
    e.preventDefault();
    applySeek(Math.max(0, (lastState ? lastState.elapsed : 0) - 5));
  } else if (e.code === "ArrowRight") {
    e.preventDefault();
    applySeek((lastState ? lastState.elapsed : 0) + 5);
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
            homeLastKey = "";
            loadHome(false);
          })
          .catch((e) => showError(e));
      }
    }
  });
} catch (_) {}

refresh();
loadLibrary();
loadProxy();
loadUserPlaylists();
loadSettings();
pollApiStatus();
showMainView("home");
loadHome();
schedulePoll();
setInterval(pollApiStatus, 3000);
setInterval(tickProgress, 250);

/// Adds one file to one of the user's playlists, creating one if asked.
async function addTrackToPlaylist(path) {
  const lists = (playlistStore.custom || []).filter((c) => c.id !== currentLocalPlaylistId);
  const items = lists.map((c) => ({
    label: `${c.title} (${(c.tracks || []).length})`,
    run: async () => {
      const added = await invoke("add_tracks_to_playlist", { id: c.id, paths: [path] });
      await loadPlaylistStore();
      if (!added) showError("этот трек уже есть в плейлисте");
    },
  }));
  items.push({
    label: "＋ Новый плейлист…",
    run: async () => {
      const title = await askPlaylistTitle("");
      if (title === null) return;
      const id = await invoke("create_playlist", { title });
      await invoke("add_tracks_to_playlist", { id, paths: [path] });
      await loadPlaylistStore();
    },
  });
  openContextMenu(lastCtxX, lastCtxY, items);
}

let lastCtxX = 0;
let lastCtxY = 0;
document.addEventListener(
  "contextmenu",
  (e) => {
    lastCtxX = e.clientX;
    lastCtxY = e.clientY;
  },
  true
);

/// Re-reads the YouTube playlist that is on screen after it was edited.
async function reloadYtPlaylist() {
  if (!currentYtPlaylistId) return;
  await loadTrackPages("get_playlist_tracks", { playlistId: currentYtPlaylistId }, () => null);
}

/// "Добавить в плейлист YouTube…" for a track row.
async function addVideoToYtPlaylist(videoId, title) {
  // The list you are looking at is where this track already is: offering it
  // makes the menu look broken.
  const lists = ytPlaylists.filter((p) => p.ytId !== currentYtPlaylistId);
  if (!lists.length) {
    showError("других плейлистов в аккаунте нет — этот трек уже здесь");
    return;
  }
  const items = lists.map((p) => ({
    label: `${playlistTitle(p)} (${p.tracks || 0})`,
    run: async () => {
      await invoke("add_playlist_tracks", { playlistId: p.ytId, videoIds: [videoId] });
      await loadPlaylistStore();
      if (currentYtPlaylistId === p.ytId) await reloadYtPlaylist();
    },
  }));
  openContextMenu(lastCtxX, lastCtxY, items);
}

/// "Добавить треки на YouTube…" from the playlist menu: offers what is on
/// screen and what is queued, because a playlist cannot be its own source.
async function pickTracksForYtPlaylist(ytId, name) {
  const sources = [];
  const shown = (currentYtTracks || []).filter((t) => t.video_id);
  if (shown.length) {
    sources.push({
      label: `Все треки на экране (${shown.length})`,
      run: async () => {
        await invoke("add_playlist_tracks", {
          playlistId: ytId,
          videoIds: shown.map((t) => t.video_id),
        });
        await loadPlaylistStore();
        if (currentYtPlaylistId === ytId) await reloadYtPlaylist();
      },
    });
  }
  const queue = (queueInfo && queueInfo.items) || [];
  if (queue.length) {
    sources.push({
      label: `Вся очередь (${queue.length})`,
      run: async () => {
        await invoke("add_playlist_tracks", {
          playlistId: ytId,
          videoIds: queue.map((t) => t.id),
        });
        await loadPlaylistStore();
        if (currentYtPlaylistId === ytId) await reloadYtPlaylist();
      },
    });
  }
  if (!sources.length) {
    showError(`нечего добавить в «${name}»: открой любой список с треками`);
    return;
  }
  openContextMenu(lastCtxX, lastCtxY, sources);
}

/* ---------- linking, copies and playlists on disk ---------- */

/// Creates the private YouTube playlist out of the rows that live there and
/// leaves the local files where they are, saying so before it starts.
async function linkPlaylistToYoutube(entry) {
  try {
    hideError();
    const preview = await invoke("playlist_link_preview", { id: entry.id });
    if (!preview.youtube) {
      showError("в этом плейлисте нет треков YouTube: отправлять нечего");
      return;
    }
    const tail =
      preview.local > 0
        ? `\n\n${preview.local} локальных файлов не будут отправлены.`
        : "";
    const ok = await askConfirm(
      `Создать приватный плейлист на YouTube из ${preview.youtube} треков?${tail}`
    );
    if (!ok) return;

    const title = await askPlaylistTitle(playlistTitle(entry));
    if (title === null) return;
    const out = await invoke("playlist_link_to_youtube", { id: entry.id, title });
    await loadPlaylistStore();
    await loadUserPlaylists();
    showError(
      `Связано с YouTube: добавлено ${out.added}` +
        (out.skipped ? `, пропущено локальных ${out.skipped}` : "")
    );
    const linked = allPlaylists().find((p) => p.id === entry.id);
    if (linked) await openSidebarPlaylist(linked);
  } catch (e) {
    showError(playlistError(e));
  }
}

/// Drops the link and keeps a local copy of what was on YouTube.
async function unlinkPlaylist(entry) {
  try {
    hideError();
    const ok = await askConfirm(
      `Отвязать «${playlistTitle(entry)}» от YouTube?\n\n` +
        "Плейлист на YouTube останется как есть; сюда вернётся его копия."
    );
    if (!ok) return;
    const added = await invoke("playlist_unlink", { id: entry.id });
    await loadPlaylistStore();
    showError(`Отвязано: в локальную копию добавлено ${added} треков`);
    const fresh = allPlaylists().find((p) => p.id === entry.id);
    if (fresh) await openSidebarPlaylist(fresh);
  } catch (e) {
    showError(playlistError(e));
  }
}

async function exportPlaylistM3u(entry) {
  try {
    hideError();
    let target = "";
    if (dialog && dialog.save) {
      target = await dialog.save({
        defaultPath: `${playlistTitle(entry) || "playlist"}.m3u`,
        filters: [{ name: "M3U", extensions: ["m3u"] }],
      });
      if (target === null) return;
    }
    const written = await invoke("playlist_export_m3u", {
      id: entry.id,
      path: target ? String(target) : "",
    });
    showError(`Сохранено: ${written}`);
  } catch (e) {
    showError(playlistError(e));
  }
}

async function importPlaylistM3u() {
  try {
    hideError();
    const file = await dialog.open({
      multiple: false,
      filters: [{ name: "M3U", extensions: ["m3u", "m3u8"] }],
    });
    if (!file) return;
    await invoke("playlist_import_m3u", { path: String(file) });
    await loadPlaylistStore();
    renderSidebar();
  } catch (e) {
    showError(playlistError(e));
  }
}

async function savePlaylistToLibrary(ytId) {
  try {
    hideError();
    await invoke("save_playlist_to_library", { playlistId: ytId });
    await loadUserPlaylists();
    await loadPlaylistStore();
    showError("плейлист сохранён в медиатеку");
  } catch (e) {
    showError(playlistError(e));
  }
}

async function copyPlaylistToAccount(ytId, title) {
  try {
    hideError();
    const name = await askPlaylistTitle(`${title || "Копия"}`);
    if (name === null) return;
    const id = await invoke("copy_playlist", { sourceId: ytId, title: name });
    await loadUserPlaylists();
    const entry = allPlaylists().find((p) => p.ytId === id);
    if (entry) await openSidebarPlaylist(entry);
    showError("копия создана");
  } catch (e) {
    showError(playlistError(e));
  }
}

/// A pasted link: the list is shown read-only, with the two ways to keep it.
async function importPlaylistByLink() {
  try {
    hideError();
    const url = await askPlaylistTitle("");
    if (!url || !url.trim()) return;
    const data = await invoke("resolve_playlist_url", { url: url.trim() });
    const rows = data.tracks || [];
    if (!rows.length) {
      showError("в этом плейлисте нет доступных треков");
      return;
    }
    // Not in the library: nothing here is editable, so the id stays empty.
    currentYtPlaylistId = null;
    currentYtPlaylistOwned = false;
    currentLocalPlaylistId = null;
    setMode("youtube");
    setYtTracks(rows, false, {
      title: "Плейлист по ссылке",
      subtitle: `${data.total || rows.length} tracks · только чтение`,
      cover: rows[0] ? rows[0].thumbnail || "" : "",
    });
    $("yt-more-btn").classList.add("hidden");
    openContextMenu(240, 200, [
      { label: "Сохранить в медиатеку", run: () => savePlaylistToLibrary(data.id) },
      {
        label: "Сделать копию…",
        run: () => copyPlaylistToAccount(data.id, "Копия плейлиста"),
      },
    ]);
  } catch (e) {
    showError(playlistError(e));
  }
}

/// "Создать плейлист…" has two homes: the disk (files) and the account.
async function askWhereToCreate() {
  openContextMenu(lastCtxX, lastCtxY, [
    {
      label: "Локальный (из файлов)",
      run: () => createCustomPlaylist(),
    },
    {
      label: "На YouTube (приватный)",
      run: async () => {
        const title = await askPlaylistTitle("");
        if (title === null) return;
        const id = await invoke("create_account_playlist", { title });
        await loadUserPlaylists();
        const entry = allPlaylists().find((p) => p.ytId === id);
        if (entry) await openSidebarPlaylist(entry);
      },
    },
    {
      label: "Импорт по ссылке…",
      run: () => importPlaylistByLink(),
    },
    {
      label: "Импорт M3U…",
      run: () => importPlaylistM3u(),
    },
  ]);
}

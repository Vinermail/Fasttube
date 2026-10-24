# Fasttube

Быстрый десктопный музыкальный плеер: локальная библиотека + каталог YouTube/YouTube Music. Tauri + Rust-ядро + изолированный Python-микросервис.

## Возможности

**Воспроизведение**
- Локальные файлы: mp3, flac, wav, ogg, m4a, opus и др.
- YouTube: поиск (веб-каталог + переключатель Music), плейлисты аккаунта, Liked, история, похожие треки
- Очередь с предзагрузкой следующих треков, шафл, повтор (off/all/one)
- Скорость воспроизведения (0.75x–2x), перемотка, громкость
- Стриминг с кэшированием: параллельная загрузка чанками (range-запросы), повторные запуски — мгновенно из кэша
- Sleep-таймер, горячие клавиши, drag&drop папки в окно

**Нормализация громкости**
- Теги ReplayGain (читаются из локальных файлов мгновенно)
- Полный анализ EBU R128 (крэйт `replaygain`, алгоритм ffmpeg) в фоне
- Быстрая предварительная оценка при первом запуске + плавная докоррекция
- Уровни: Off / Light / Medium / Strong

**Библиотека**
- Плейлисты из структуры папок, метаданные и обложки из тегов (lofty)
- Избранное, история прослушивания
- Фильтр и сортировка списка, виртуализация (тысячи треков без лагов)

**Прочее**
- Синхронизированные тексты песен (lrclib.net) с подсветкой
- Панель очереди (переход, удаление, очистка)
- Два источника YouTube: OAuth (device flow) и browser-auth (куки)

## Архитектура

```
┌──────────────┐  IPC (команды + события)  ┌──────────────────────────────┐
│ WebView (UI) │ ────────────────────────▶ │ Rust-ядро (Tauri)             │
│ HTML/CSS/JS  │ ◀──────────────────────── │ ├ движок rodio/symphonia      │
└──────────────┘                           │ ├ очередь + prefetch          │
                                           │ ├ downloader (reqwest, range) │
                                           │ ├ кэш/настройки/история      │
                                           │ └ сайдкар-супервизор          │
                                           └──────────────┬───────────────┘
                                                          │ gRPC (localhost, динамич. порт)
                                           ┌──────────────▼───────────────┐
                                           │ Python-сервис (api/)          │
                                           │ ├ yt-dlp: resolve/search      │
                                           │ ├ ytmusicapi: плейлисты/auth │
                                           │ ├ lrclib: тексты              │
                                           │ └ прокси/DoH-обходы           │
                                           └───────────────────────────────┘
```

- **Rust-ядро** само спавнит Python-сайдкар (`uv`/venv), следит за ним (health-check, рестарт) и убивает при выходе
- **gRPC** (tonic ↔ grpcio) по `proto/player_api.proto` — аудио по нему не гоняется, только метаданные и URL потоков
- **Плеер** декодирует AAC-стримы (itag 140/141) прогрессивно, качает параллельными range-запросами

## Требования

- Rust (stable, 2024 edition)
- Python 3.11+ и [`uv`](https://docs.astral.sh/uv/) — для сайдкара
- Windows 10/11 (WebView2), macOS/Linux — в теории, тестировалось на Windows
- YouTube доступен только через прокси/VPN (см. ниже)

## Запуск

```bash
# 1. Python-окружение (один раз)
uv sync --project api

# 2. Сборка и запуск
cargo run
# или сразу с папкой музыки:
cargo run -- "C:\путь\к\музыке"
```

Генерация gRPC-стабов после изменения `proto/player_api.proto`:

```bash
uv run --project api python -m grpc_tools.protoc -I proto \
  --python_out=api/src/ytapi/pb --grpc_python_out=api/src/ytapi/pb proto/player_api.proto
```

## Авторизация YouTube

**Browser-auth (рекомендуется, работает сейчас):**
1. Открой `music.youtube.com` в браузере, залогинься
2. F12 → Network → обнови страницу → найди запрос `https://music.youtube.com/youtubei/v1/browse`
3. ПКМ → Copy → Copy request headers (или Copy as cURL)
4. В приложении: ⚙ Settings → YouTube account → вставь → Use browser auth

Куки живут пару недель, потом повторить.

**OAuth (device flow):**
1. Google Cloud Console → проект → включи YouTube Data API v3
2. Credentials → OAuth client ID → тип **«TVs and Limited Input devices»**
3. В приложении вставь Client ID/Secret → Start Google login → открой ссылку → подтверди

> ⚠️ OAuth-эндпоинты ytmusicapi сейчас частично сломаны на стороне Google
> ([issue #813](https://github.com/sigma67/ytmusicapi/issues/813)) — потому и нужен browser-auth.

## Настройки

- **Audio quality**: Standard (AAC 128kbps) / High (до 256+ kbps с Premium)
- **Normalization level**: Off / Light / Medium / Strong (целевая громкость)
- **Sleep timer**, **Clear download cache**

## Горячие клавиши

| Клавиша | Действие |
|---|---|
| `Space` | Play/Pause |
| `←` / `→` | Перемотка ±5с |
| `↑` / `↓` | Громкость ±5% |
| `Ctrl+←` / `Ctrl+→` | Предыдущий/следующий трек |

## Данные

Всё хранится в `%LOCALAPPDATA%\fasttube\`:

- `cache/` — скачанные треки (`<video_id>.m4a`) и обложки
- `settings.json`, `history.json`, `favorites.json`, `loudness.json`
- При первом запуске данные из `player_llm\` (прежнее имя) мигрируют автоматически

Секреты авторизации (`api/oauth.json`, `api/browser.json`, `api/oauth_creds.json`) **не коммитятся** (см. `.gitignore`).

## Сеть и прокси

YouTube в некоторых сетях режется по SNI (TLS-хендшейк зависает, DNS отдаёт только IPv6).
Сервис умеет:

- автодетект локального прокси (порты 10808/1080/7890) или переменную `YTAPI_PROXY`
- DoH-фолбэк (Cloudflare) для IPv4-резолва

googlevideo отдаёт данные только с заголовком `Range` (иначе ~30 KB/s) — учтено в загрузчике.

## Структура

```
├── proto/player_api.proto     # контракт gRPC
├── src/                       # Rust-ядро (движок, очередь, загрузчик, сайдкар)
├── api/                       # Python-сервис (yt-dlp, ytmusicapi, lrclib)
├── ui/                        # фронтенд (vanilla JS, встраивается в бинарь)
├── capabilities/              # ACL Tauri
└── tauri.conf.json
```

## Лицензия

GPL-3.0 — см. [LICENSE](LICENSE).

Проект использует YouTube только через публичные API и официальные средства (yt-dlp, ytmusicapi); используй на свой страх и риск согласно правилам сервисов Google.

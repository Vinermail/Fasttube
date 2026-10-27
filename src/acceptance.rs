//! Manual acceptance: the real sidecar, the real account, the real playlist.
//!
//! These tests drive the same code the commands drive (`SyncEngine` over
//! `GrpcApi`), so what they print is what happens when the app is used by hand.
//! They are `#[ignore]`d because they need the sidecar, a signed-in account and
//! the network; run them with:
//!
//! ```text
//! cargo test --bin fasttube manual_acceptance -- --ignored --nocapture
//! ```
//!
//! Everything happens in a private playlist the test creates and deletes; the
//! only account-wide change is a like that is put back the way it was.

use crate::sync::model::{OpKind, SnapshotTrack};
use crate::sync::store::SnapshotStore;
use crate::sync::worker::{SyncEngine, SyncEvent};
use crate::sync::{GrpcApi, Op};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn python() -> PathBuf {
    let venv = project_root()
        .join("api")
        .join(".venv")
        .join("Scripts")
        .join("python.exe");
    if venv.exists() {
        venv
    } else {
        PathBuf::from("python")
    }
}

/// A runtime of our own: `tauri::async_runtime` needs an app to have started.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a tokio runtime")
    })
}

fn block<F, T>(f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    runtime().block_on(f)
}

/// A sidecar on its own port, with a token this test knows.
struct Sidecar {
    child: Child,
    port: u16,
    token: String,
}

impl Sidecar {
    fn start(port: Option<u16>) -> Self {
        let token = format!("acceptance-{}", std::process::id());
        let mut cmd = Command::new(python());
        cmd.args(["-m", "ytapi.server"])
            .env("FASTTUBE_API_TOKEN", &token)
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUNBUFFERED", "1")
            .current_dir(project_root())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        if let Some(port) = port {
            cmd.env("YTAPI_PORT", port.to_string());
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().expect("spawn the sidecar");
        let stdout = child.stdout.take().expect("sidecar stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut handshake = Some(tx);
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(tx) = handshake.take() {
                    let port = line
                        .strip_prefix("PORT=")
                        .and_then(|p| p.trim().parse::<u16>().ok());
                    let _ = tx.send(port);
                }
            }
            if let Some(tx) = handshake.take() {
                let _ = tx.send(None);
            }
        });
        let port = rx
            .recv_timeout(Duration::from_secs(40))
            .expect("handshake in time")
            .expect("sidecar printed a port");
        Sidecar {
            child,
            port,
            token,
        }
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn real_clock() -> Box<dyn Fn() -> f64 + Send + Sync> {
    Box::new(crate::now_secs)
}

fn say(line: &str) {
    println!("[acceptance] {line}");
}

/// Spins until the journal is empty or the deadline passes.
fn drain<A: crate::sync::api::PlaylistApi>(
    engine: &mut SyncEngine<A>,
    seconds: u64,
) -> (usize, Vec<SyncEvent>) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut ticks = 0;
    let mut all = Vec::new();
    while Instant::now() < deadline {
        let events = engine.tick();
        ticks += 1;
        all.extend(events);
        std::thread::sleep(Duration::from_millis(350));
        if engine.pending_ops() == 0 {
            break;
        }
    }
    (ticks, all)
}

fn store_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fasttube-acceptance-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// First row of the first owned playlist: a real video id with a real title.
fn a_track_of_theirs() -> (String, String, String) {
    let lists = block(crate::grpc::get_user_playlists()).expect("user playlists");
    for list in lists.playlists.iter().take(10) {
        let page = block(crate::grpc::get_playlist_tracks(&list.id, 0, 5)).expect("tracks");
        if !page.owned || page.error == "sign in to YouTube in the app" {
            continue;
        }
        if let Some(track) = page.tracks.first() {
            return (track.video_id.clone(), track.title.clone(), track.artist.clone());
        }
    }
    panic!("no owned playlist with a track to test with");
}

#[test]
#[ignore = "needs the sidecar, a signed-in account and the network"]
fn manual_acceptance() {
    let mut sidecar = Sidecar::start(None);
    // The channel is a tokio type: build it inside the runtime.
    {
        let _enter = runtime().enter();
        crate::grpc::set_endpoint(sidecar.port, sidecar.token.clone()).expect("endpoint");
    }
    say(&format!("sidecar on 127.0.0.1:{}", sidecar.port));

    let bundle = crate::secrets::load().expect("a saved session in auth.bin");
    let status = block(crate::grpc::set_session(
        &bundle.cookie,
        &bundle.user_agent,
        &bundle.x_goog_authuser,
    ))
    .expect("set_session");
    let account = status.account_name.clone();
    say(&format!(
        "session: state={:?} account={}",
        status.state(),
        if account.is_empty() { "?" } else { &account }
    ));
    assert_eq!(
        status.state(),
        crate::grpc::SessionState::Active,
        "the acceptance needs a live session"
    );

    // Leftovers of an earlier run that died before its cleanup.
    if let Ok(lists) = block(crate::grpc::get_user_playlists()) {
        for list in lists
            .playlists
            .iter()
            .filter(|l| l.name.starts_with("fasttube acceptance"))
        {
            let gone = block(crate::grpc::delete_account_playlist(&list.id));
            say(&format!(
                "leftover playlist {} deleted: {:?}",
                list.id,
                gone.map(|r| r.ok)
            ));
        }
    }

    let (video_id, title, artist) = a_track_of_theirs();
    say(&format!("test track: {video_id} — {artist} — {title}"));

    // ---------------------------------------------------------------- setup --
    let name = format!(
        "fasttube acceptance {}",
        chrono_like_stamp()
    );
    let created = block(crate::grpc::create_account_playlist(&name)).expect("create");
    assert!(created.ok && !created.id.is_empty(), "created: {created:?}");
    let playlist_id = created.id.clone();
    say(&format!("created a private playlist: {playlist_id}"));

    let dir = store_dir("main");
    let store = SnapshotStore::new(dir.clone());
    let mut engine = SyncEngine::new(GrpcApi, store.clone(), account.clone(), real_clock());
    let first = engine.revalidate(&playlist_id);
    say(&format!("first read events: {first:?}"));
    say(&format!(
        "snapshot after the first read: {:?}",
        engine.snapshot(&playlist_id).map(|s| s.tracks.len())
    ));

    // ------------------------------------------------- S1: add through the journal --
    engine.enqueue(Op::new(
        OpKind::Add,
        &playlist_id,
        "",
        serde_json::json!({"items": [{
            "video_id": video_id,
            "temp_id": "pending:acceptance-1",
            "title": title,
            "artist": artist,
        }]}),
        crate::now_secs(),
    ));
    let (ticks, events) = drain(&mut engine, 30);
    say(&format!("S1 ticks={ticks} events={events:?}"));
    say(&format!(
        "S1 status={:?} pending={}",
        engine.status(&playlist_id),
        engine.pending_ops()
    ));
    let live = block(crate::grpc::get_playlist_tracks(&playlist_id, 0, 50)).expect("read");
    say(&format!(
        "S1 live read right after the tick: {} row(s), error={:?}",
        live.tracks.len(),
        live.error
    ));
    let after_add = engine.snapshot(&playlist_id).expect("snapshot");
    let row = after_add
        .tracks
        .iter()
        .find(|t| t.video_id == video_id)
        .expect("the row is in the snapshot");
    say(&format!(
        "S1 snapshot row: set_video_id={:?} temp_id={:?}",
        row.set_video_id, row.temp_id
    ));
    assert!(!row.set_video_id.is_empty(), "YouTube gave the row an id");
    assert!(row.temp_id.is_empty(), "the temporary id was replaced");

    // Independent read: this is what the web and the phone see.
    let fresh = block(crate::grpc::get_playlist_tracks(&playlist_id, 0, 50)).expect("fresh read");
    let server_has_it = fresh.tracks.iter().any(|t| t.video_id == video_id);
    say(&format!(
        "S1 server-side (web/phone) sees the track: {server_has_it} (total={})",
        fresh.total
    ));
    assert!(server_has_it, "the add really happened on YouTube");

    // ------------------------------------------------- S1b: undo of a removal --
    let before_undo = engine.snapshot(&playlist_id).unwrap().tracks.len();
    let mut remove = Op::new(
        OpKind::Remove,
        &playlist_id,
        "",
        serde_json::json!({
            "video_id": video_id,
            "set_video_id": row.set_video_id,
            "title": title,
            "artist": artist,
            "duration": 0,
            "thumbnail": "",
            "like_status": "",
            "index": 0,
        }),
        crate::now_secs(),
    );
    remove.not_before = crate::now_secs() + 8.0;
    let op_id = remove.id.clone();
    let calls_before = 0;
    engine.enqueue(remove);
    engine.tick();
    say(&format!(
        "S1b deferred removal: snapshot {} -> {} rows, no request expected",
        before_undo,
        engine.snapshot(&playlist_id).unwrap().tracks.len()
    ));
    assert_eq!(
        engine.snapshot(&playlist_id).unwrap().tracks.len(),
        before_undo - 1,
        "the row disappears from the snapshot at once"
    );
    let taken = engine.cancel(&op_id).expect("the operation is still pending");
    assert_eq!(taken.kind, OpKind::Remove);
    assert!(engine.restore_row(
        &playlist_id,
        0,
        SnapshotTrack {
            video_id: video_id.clone(),
            set_video_id: row.set_video_id.clone(),
            title: title.clone(),
            artist: artist.clone(),
            ..Default::default()
        },
    ));
    say(&format!(
        "S1b undo: snapshot back to {} rows",
        engine.snapshot(&playlist_id).unwrap().tracks.len()
    ));
    assert_eq!(
        engine.snapshot(&playlist_id).unwrap().tracks.len(),
        before_undo
    );
    let still_there = block(crate::grpc::get_playlist_tracks(&playlist_id, 0, 50)).expect("read");
    assert!(
        still_there.tracks.iter().any(|t| t.video_id == video_id),
        "undo sent nothing, so YouTube still has the row"
    );
    say("S1b YouTube unchanged, as expected for an undo");
    let _ = calls_before;

    // ------------------------------- S2: a change made elsewhere shows up as +1 --
    let second = fresh
        .tracks
        .iter()
        .map(|t| t.video_id.clone())
        .find(|id| id != &video_id)
        .unwrap_or_else(|| "dQw4w9WgXcQ".to_string());
    let added = block(crate::grpc::add_playlist_items_full(
        &playlist_id,
        &[second.clone()],
        true,
    ))
    .expect("add as if from the phone");
    say(&format!(
        "S2 added {second} from \"the phone\": ok={}",
        added.ok
    ));
    let before = engine.snapshot(&playlist_id).unwrap().tracks.len();
    let events = engine.revalidate(&playlist_id);
    let diff = events.iter().find_map(|e| match e {
        SyncEvent::Synced { id, diff } if id == &playlist_id => Some(diff.clone()),
        _ => None,
    });
    let after = engine.snapshot(&playlist_id).unwrap().tracks.len();
    say(&format!(
        "S2 revalidation: {before} -> {after} rows, diff={diff:?} (the UI toasts +N from this)"
    ));
    assert_eq!(after, before + 1, "the revalidation picked the new row up");
    assert_eq!(diff.map(|d| d.added), Some(1), "and reported exactly one add");

    // ------------------------------------------------ S3: offline, then back online --
    // The per-item id of the row added "from the phone", so the deferred removal
    // can really go through once the network is back.
    let second_row = block(crate::grpc::get_playlist_tracks(&playlist_id, 0, 50))
        .expect("read")
        .tracks
        .into_iter()
        .find(|t| t.video_id == second)
        .expect("the second track is on YouTube");
    let second_set_id = second_row.set_video_id.clone();
    sidecar.stop();
    say("S3 sidecar killed: the app is offline now");
    engine.enqueue(Op::new(
        OpKind::Remove,
        &playlist_id,
        "",
        serde_json::json!({
            "video_id": second,
            "set_video_id": second_set_id,
            "title": second_row.title,
            "artist": second_row.artist,
            "index": 1,
        }),
        crate::now_secs(),
    ));
    engine.tick();
    let status = engine.status(&playlist_id);
    say(&format!(
        "S3 status while offline: state={:?} pending={} error={:?}",
        status.state, status.pending, status.error
    ));
    assert_eq!(status.pending, 1, "one change waits for the network");

    let pending_before_restart = engine.pending_ops();
    drop(engine);
    say(&format!(
        "S3 restart: {pending_before_restart} operation(s) left in the journal"
    ));

    let mut restarted = Sidecar::start(Some(sidecar.port));
    {
        let _enter = runtime().enter();
        crate::grpc::set_endpoint(restarted.port, restarted.token.clone()).ok();
    }
    // The session lives in the sidecar's memory, so it has to be installed again.
    let status = block(crate::grpc::set_session(
        &bundle.cookie,
        &bundle.user_agent,
        &bundle.x_goog_authuser,
    ))
    .expect("set_session after restart");
    assert_eq!(status.state(), crate::grpc::SessionState::Active);
    say("S3 sidecar is back with the same port and a fresh session");

    let mut engine = SyncEngine::new(GrpcApi, store.clone(), account.clone(), real_clock());
    let replayed = engine.replay();
    say(&format!(
        "S3 replay after the restart: {} event(s), {} op(s) in the journal",
        replayed.len(),
        engine.pending_ops()
    ));
    let (ticks, _) = drain(&mut engine, 60);
    say(&format!(
        "S3 drained after {ticks} ticks, {} op(s) left",
        engine.pending_ops()
    ));
    assert_eq!(engine.pending_ops(), 0, "everything went out");
    let now_remote = block(crate::grpc::get_playlist_tracks(&playlist_id, 0, 50)).expect("read");
    let second_gone = !now_remote.tracks.iter().any(|t| t.video_id == second);
    say(&format!(
        "S3 after the drain YouTube has {} row(s); the queued removal went through: {second_gone}",
        now_remote.tracks.len()
    ));
    assert!(second_gone, "the operation that waited for the network was applied");

    // ------------------------------------------------- S4: duplicate handling (live) --
    let dup = block(crate::grpc::add_playlist_items_full(
        &playlist_id,
        &[video_id.clone()],
        false,
    ))
    .expect("duplicate add");
    say(&format!(
        "S4 allow_duplicates=false: ok={} error={:?}",
        dup.ok, dup.error
    ));

    // ------------------------------------------------------- S5: someone else's list --
    let lists = block(crate::grpc::get_user_playlists()).expect("lists");
    let mut foreign = None;
    for list in lists.playlists.iter().take(25) {
        if list.id == playlist_id {
            continue;
        }
        let page = block(crate::grpc::get_playlist_tracks(&list.id, 0, 1)).ok();
        if let Some(page) = page
            && !page.owned
            && page.error.is_empty()
        {
            foreign = Some(list.id.clone());
            break;
        }
    }
    match foreign {
        Some(id) => {
            let read = block(crate::grpc::get_playlist_tracks(&id, 0, 3)).expect("read");
            let write = block(crate::grpc::add_playlist_items_full(&id, &[video_id.clone()], true))
                .expect("the call answers");
            say(&format!(
                "S5 foreign playlist {id}: owned={} write ok={} error={:?}",
                read.owned, write.ok, write.error
            ));
            assert!(!read.owned);
            assert!(!write.ok, "a foreign playlist refuses writes");
        }
        None => say("S5 no foreign playlist saved in the account: skipped"),
    }

    // --------------------------------------------------------- S6: another account --
    // A journal written by this account, then the app started as someone else.
    let switch_dir = store_dir("switch");
    let switch_store = SnapshotStore::new(switch_dir.clone());
    let mut pending = Op::new(
        OpKind::Add,
        &playlist_id,
        "",
        serde_json::json!({"items": [{"video_id": second, "temp_id": "pending:x"}]}),
        crate::now_secs(),
    );
    pending.account_id = account.clone();
    switch_store.save_journal(&[pending]).expect("journal");
    let mut stranger = SyncEngine::new(
        GrpcApi,
        switch_store.clone(),
        "someone-else".to_string(),
        real_clock(),
    );
    let events = stranger.replay();
    let foreign = events
        .iter()
        .filter(|e| matches!(e, SyncEvent::ForeignOps { .. }))
        .count();
    stranger.tick();
    say(&format!(
        "S6 journal of {account:?} while signed in as someone-else: {} foreign notification(s), {} op(s) kept, snapshot shown: {}",
        foreign,
        stranger.pending_ops(),
        stranger.snapshot(&playlist_id).is_some()
    ));
    assert_eq!(stranger.pending_ops(), 1, "not applied, not dropped");
    assert!(
        stranger.snapshot(&playlist_id).is_none(),
        "another account's snapshot is not shown"
    );
    let _ = std::fs::remove_dir_all(&switch_dir);

    // ---------------------------------------------------------------- S7: likes --
    // The row's like status as YouTube reports it, read fresh every time.
    let like_now = |id: &str| -> String {
        block(crate::grpc::get_playlist_tracks_fresh(&playlist_id, 0, 50))
            .map(|page| {
                page.tracks
                    .iter()
                    .find(|t| t.video_id == id)
                    .map(|t| t.like_status.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    };

    let original = like_now(&video_id);
    say(&format!("S7 the track's like status now: {original:?}"));
    let flip = if original == "LIKE" { "INDIFFERENT" } else { "LIKE" };
    let expect_flip = if flip == "LIKE" { "LIKE" } else { "" };

    block(crate::grpc::rate_song(&video_id, flip)).expect("rate");
    let after = like_now(&video_id);
    say(&format!(
        "S7 after {flip}: fresh read says {after:?} (expected {expect_flip:?})"
    ));
    assert_eq!(after, expect_flip, "a like made here is visible on YouTube");

    let back = if original == "LIKE" { "LIKE" } else { "INDIFFERENT" };
    block(crate::grpc::rate_song(&video_id, back)).expect("rate back");
    let restored = like_now(&video_id);
    say(&format!(
        "S7 put back to {back}: fresh read says {restored:?} (was {original:?})"
    ));
    assert_eq!(restored, original, "the account is left as it was found");

    // ---------------------------------------------------------------- cleanup --
    let deleted = block(crate::grpc::delete_account_playlist(&playlist_id)).expect("delete");
    say(&format!(
        "cleanup: test playlist deleted ok={} error={:?}",
        deleted.ok, deleted.error
    ));
    let _ = std::fs::remove_dir_all(&dir);
    restarted.stop();
    let _ = &mut sidecar;
}

/// `HH:MM` without pulling in a date crate.
fn chrono_like_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let time = now % 86_400;
    format!("{:02}:{:02}", time / 3600, (time % 3600) / 60)
}

/// Keeps the clock import honest in case a future edit drops it.
#[allow(dead_code)]
fn unused_atomic() -> Arc<AtomicU64> {
    let value = Arc::new(AtomicU64::new(0));
    let _ = value.fetch_add(1, Ordering::Relaxed);
    value
}

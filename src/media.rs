use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use glib::Sender as GlibSender;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedValue;

use crate::model::{AppEvent, MediaCommand, MediaSnapshot, SharedMedia};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

#[derive(Default)]
struct PlayerProps {
    status: String,
    meta: HashMap<String, OwnedValue>,
    volume: f64,
    position_us: i64,
}

fn session() -> Option<Connection> {
    Connection::session().ok()
}

fn list_names(conn: &Connection) -> Vec<String> {
    let reply = conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "ListNames",
        &(),
    );
    match reply {
        Ok(msg) => msg.body().deserialize::<Vec<String>>().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn bus_pid(conn: &Connection, name: &str) -> Option<u32> {
    let reply = conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "GetConnectionUnixProcessID",
        &(name,),
    );
    match reply {
        Ok(msg) => msg.body().deserialize::<u32>().ok(),
        Err(_) => None,
    }
}

fn player_proxy<'a>(conn: &Connection, name: &'a str) -> Option<Proxy<'a>> {
    Proxy::new(conn, name, MPRIS_PATH, PLAYER_IFACE).ok()
}

fn read_props(
    conn: &Connection,
    name: &str,
) -> Option<(PlayerProps, Option<zbus::zvariant::OwnedObjectPath>)> {
    let proxy = player_proxy(conn, name)?;
    let status: String = proxy.get_property("PlaybackStatus").ok()?;
    let meta: HashMap<String, OwnedValue> = proxy.get_property("Metadata").unwrap_or_default();
    let volume: f64 = proxy.get_property("Volume").unwrap_or(1.0);
    let position_us: i64 = proxy.get_property("Position").unwrap_or(0);
    let track_id = meta
        .get("mpris:trackid")
        .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v.clone()).ok());
    Some((
        PlayerProps {
            status,
            meta,
            volume,
            position_us,
        },
        track_id,
    ))
}

fn score(props: &PlayerProps) -> i32 {
    let mut score = match props.status.as_str() {
        "Playing" => 3,
        "Paused" => 2,
        _ => 1,
    };
    if props.meta.contains_key("xesam:title") {
        score += 1;
    }
    score
}

fn select_player(conn: &Connection, skip: Option<&str>) -> Option<String> {
    let mut names: Vec<String> = list_names(conn)
        .into_iter()
        .filter(|n| n.starts_with(MPRIS_PREFIX) && Some(n.as_str()) != skip)
        .collect();
    names.sort();
    let mut best: Option<(String, i32)> = None;
    for name in names {
        if let Some((props, _)) = read_props(conn, &name) {
            let s = score(&props);
            if best.as_ref().map(|(_, b)| s > *b).unwrap_or(true) {
                best = Some((name, s));
            }
        }
    }
    best.map(|(name, _)| name)
}

fn meta_string(meta: &HashMap<String, OwnedValue>, key: &str) -> String {
    meta.get(key)
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default()
}

fn meta_artists(meta: &HashMap<String, OwnedValue>) -> String {
    meta.get("xesam:artist")
        .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
        .map(|v| v.join(", "))
        .unwrap_or_default()
}

fn meta_i64(meta: &HashMap<String, OwnedValue>, key: &str) -> i64 {
    meta.get(key)
        .and_then(|v| i64::try_from(v.clone()).ok())
        .unwrap_or(0)
}

fn load_art(url: &str) -> Option<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).ok();
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let mut res = ureq::get(url).call().ok()?;
        let data = res.body_mut().read_to_vec().ok()?;
        if data.is_empty() {
            None
        } else {
            Some(data)
        }
    } else {
        None
    }
}

pub fn spawn(events: GlibSender<AppEvent>, commands: Receiver<MediaCommand>) -> SharedMedia {
    let shared = SharedMedia::default();
    let shared_thread = shared.clone();

    thread::spawn(move || {
        let mut conn = session();
        let mut current: Option<String> = None;
        let mut current_track: Option<zbus::zvariant::OwnedObjectPath> = None;
        let mut last_props: Option<PlayerProps> = None;
        let mut art_seen: HashSet<String> = HashSet::new();
        let mut known_names: Option<Vec<String>> = None;
        let mut next_poll = Instant::now();

        loop {
            match commands.recv_timeout(Duration::from_millis(40)) {
                Ok(cmd) => {
                    if conn.is_none() {
                        conn = session();
                    }
                    if let (Some(c), Some(name)) = (conn.as_ref(), current.as_ref()) {
                        apply_command(c, name, &mut last_props, current_track.clone(), cmd);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            if Instant::now() < next_poll {
                continue;
            }
            next_poll = Instant::now() + Duration::from_millis(500);

            if conn.is_none() {
                conn = session();
            }
            let Some(conn) = conn.as_ref() else {
                let _ = events.send(AppEvent::Media(MediaSnapshot::default()));
                shared_thread.set(None, None);
                continue;
            };

            let names = list_names(conn);
            let current_is_valid = current
                .as_ref()
                .map(|n| names.iter().any(|x| x == n))
                .unwrap_or(false);

            let mut players: Vec<String> = names
                .iter()
                .filter(|n| n.starts_with(MPRIS_PREFIX))
                .cloned()
                .collect();
            players.sort();
            let players_changed = known_names.as_ref() != Some(&players);
            if players_changed {
                known_names = Some(players);
            }

            if !current_is_valid || players_changed {
                let selected = select_player(conn, None);
                if selected != current {
                    last_props = None;
                    current_track = None;
                }
                current = selected;
            }

            let Some(name) = current.clone() else {
                let _ = events.send(AppEvent::Media(MediaSnapshot::default()));
                shared_thread.set(None, None);
                continue;
            };

            let Some((props, track)) = read_props(conn, &name) else {
                current = None;
                last_props = None;
                current_track = None;
                let _ = events.send(AppEvent::Media(MediaSnapshot::default()));
                shared_thread.set(None, None);
                continue;
            };

            let pid = bus_pid(conn, &name);
            shared_thread.set(Some(name.clone()), pid);
            current_track = track;
            last_props = Some(PlayerProps {
                status: props.status.clone(),
                meta: props.meta.clone(),
                volume: props.volume,
                position_us: props.position_us,
            });

            let title = meta_string(&props.meta, "xesam:title");
            let artist = meta_artists(&props.meta);
            let album = meta_string(&props.meta, "xesam:album");
            let art_url = meta_string(&props.meta, "mpris:artUrl");
            let length = meta_i64(&props.meta, "mpris:length") / 1_000_000;

            if !art_url.is_empty() && art_seen.insert(art_url.clone()) {
                let tx = events.clone();
                let url = art_url.clone();
                thread::spawn(move || {
                    let data = load_art(&url);
                    let _ = tx.send(AppEvent::Art { url, data });
                });
            }

            let snapshot = MediaSnapshot {
                bus_name: Some(name),
                playing: props.status == "Playing",
                title,
                artist,
                album,
                art_url,
                length,
                position: props.position_us as f64 / 1_000_000.0,
            };
            let _ = events.send(AppEvent::Media(snapshot));
        }
    });

    shared
}

fn apply_command(
    conn: &Connection,
    name: &str,
    props: &mut Option<PlayerProps>,
    track: Option<zbus::zvariant::OwnedObjectPath>,
    cmd: MediaCommand,
) {
    let Some(proxy) = player_proxy(conn, name) else {
        return;
    };
    match cmd {
        MediaCommand::PlayPause => {
            let _ = proxy.call::<_, _, ()>("PlayPause", &());
        }
        MediaCommand::Next => {
            let _ = proxy.call::<_, _, ()>("Next", &());
        }
        MediaCommand::Previous => {
            let _ = proxy.call::<_, _, ()>("Previous", &());
        }
        MediaCommand::Seek(seconds) => {
            let pos_us = (seconds.max(0.0) * 1_000_000.0) as i64;
            let mut done = false;
            if let Some(track) = track.clone() {
                if proxy
                    .call::<_, _, ()>("SetPosition", &(track, pos_us))
                    .is_ok()
                {
                    done = true;
                }
            }
            if !done {
                let current_us = props.as_ref().map(|p| p.position_us).unwrap_or(0);
                let _ = proxy.call::<_, _, ()>("Seek", &(pos_us - current_us));
            }
            if let Some(p) = props.as_mut() {
                p.position_us = pos_us;
            }
        }
        MediaCommand::NudgeVolume(delta) => {
            let current = proxy.get_property::<f64>("Volume").unwrap_or(1.0);
            let v = (current + delta).clamp(0.0, 1.0);
            let _ = proxy.set_property("Volume", v);
            if let Some(p) = props.as_mut() {
                p.volume = v;
            }
        }
    }
}

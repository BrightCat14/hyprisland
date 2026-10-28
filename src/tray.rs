use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use glib::Sender as GlibSender;
use zbus::blocking::{Connection, Proxy};
use zbus::interface;
use zbus::message::Header;
use zbus::zvariant::OwnedValue;

use crate::model::{AppEvent, TrayCommand, TrayIcon, TraySnapshot};

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const AYATANA_IFACE: &str = "com.canonical.indicator.application";
const ITEM_PATHS: [&str; 2] = ["/StatusNotifierItem", "/org/ayatana/StatusNotifierItem"];

#[derive(Clone, Default)]
struct SniWatcher {
    items: Arc<Mutex<Vec<String>>>,
}

#[allow(clippy::unused_async)]
#[interface(name = "org.kde.StatusNotifierWatcher")]
impl SniWatcher {
    fn register_status_notifier_item(&self, service: String, #[zbus(header)] header: Header<'_>) {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let ident = resolve_registration(&service, &sender);
        if let Some(ident) = ident {
            let mut items = self.items.lock().unwrap();
            if !items.contains(&ident) {
                items.push(ident);
            }
        }
    }

    fn register_status_notifier_host(&self, _service: String) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().unwrap().clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

fn resolve_registration(service: &str, sender: &str) -> Option<String> {
    let service = service.trim();
    let sender = sender.trim();
    if service.is_empty() {
        if sender.is_empty() {
            return None;
        }
        return Some(format!("{sender}/StatusNotifierItem"));
    }
    if service.starts_with('/') {
        if sender.is_empty() {
            return None;
        }
        return Some(format!("{sender}{service}"));
    }
    if service.contains('/') {
        return Some(service.to_string());
    }
    Some(format!("{service}/StatusNotifierItem"))
}

fn split_registration(service: &str) -> Option<(String, String)> {
    let service = service.trim();
    if service.is_empty() {
        return None;
    }
    if let Some((name, rest)) = service.split_once('/') {
        Some((name.to_string(), format!("/{rest}")))
    } else {
        Some((service.to_string(), "/StatusNotifierItem".to_string()))
    }
}

fn name_has_owner(conn: &Connection, name: &str) -> bool {
    let reply = conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "NameHasOwner",
        &(name,),
    );
    match reply {
        Ok(msg) => msg.body().deserialize::<bool>().unwrap_or(false),
        Err(_) => false,
    }
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

fn pretty_title(raw: &str) -> String {
    let mut t = raw.replace("_status_icon", "").replace("-status-icon", "");
    if t.contains('_') && !t.contains(' ') {
        t = t.split('_').next().unwrap_or("").to_string();
    }
    t = t.replace('_', " ").trim().to_string();
    if let Some(first) = t.chars().next() {
        if first.is_lowercase() {
            t = format!("{}{}", first.to_uppercase(), &t[first.len_utf8()..]);
        }
    }
    if t.is_empty() {
        raw.to_string()
    } else {
        t
    }
}

fn as_string(value: &OwnedValue) -> Option<String> {
    if let Ok(s) = String::try_from(value.clone()) {
        return Some(s);
    }
    if let Ok(p) = zbus::zvariant::OwnedObjectPath::try_from(value.clone()) {
        return Some(p.to_string());
    }
    None
}

fn icon_from_pixmap(value: &OwnedValue) -> Option<TrayIcon> {
    let entries: Vec<(i32, i32, Vec<u8>)> = Vec::try_from(value.clone()).ok()?;
    let (width, height, data) = entries
        .into_iter()
        .max_by_key(|(w, h, _)| (*w as i64) * (*h as i64))?;
    if width <= 0 || height <= 0 {
        return None;
    }
    let need = (width as usize) * (height as usize) * 4;
    if data.len() < need {
        return None;
    }
    let mut rgba = data[..need].to_vec();
    let (pixels, _) = rgba.as_chunks_mut::<4>();
    for px in pixels {
        let a = px[0];
        let r = px[1];
        let g = px[2];
        let b = px[3];
        px[0] = r;
        px[1] = g;
        px[2] = b;
        px[3] = a;
    }
    Some(TrayIcon::Raw {
        width,
        height,
        data: rgba,
    })
}

struct TrayItem {
    snapshot: TraySnapshot,
    bus_name: String,
    path: String,
    iface: String,
}

fn read_item(conn: &Connection, name: &str, path: &str) -> Option<TrayItem> {
    for iface in [ITEM_IFACE, AYATANA_IFACE] {
        let Ok(props) = Proxy::new(conn, name, path, "org.freedesktop.DBus.Properties") else {
            continue;
        };
        let all: HashMap<String, OwnedValue> = match props.call("GetAll", &(iface,)) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let title_raw = all
            .get("Title")
            .and_then(as_string)
            .filter(|s| !s.is_empty())
            .or_else(|| all.get("Id").and_then(as_string))
            .unwrap_or_default();
        if title_raw.is_empty() && !all.contains_key("IconPixmap") && !all.contains_key("IconName")
        {
            continue;
        }

        let icon = all
            .get("IconPixmap")
            .and_then(icon_from_pixmap)
            .or_else(|| {
                all.get("IconName")
                    .and_then(as_string)
                    .filter(|s| !s.is_empty())
                    .map(TrayIcon::Named)
            });

        let has_menu = all
            .get("Menu")
            .and_then(as_string)
            .map(|s| !s.is_empty() && s != "/")
            .unwrap_or(false);

        let title = pretty_title(&title_raw);
        if title.is_empty() && icon.is_none() && !has_menu {
            continue;
        }

        return Some(TrayItem {
            snapshot: TraySnapshot {
                ident: format!("{name}{path}"),
                title,
                icon,
            },
            bus_name: name.to_string(),
            path: path.to_string(),
            iface: iface.to_string(),
        });
    }
    None
}

fn watched_registrations(conn: &Connection, own: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    let mut out = own.lock().unwrap().clone();
    if let Ok(proxy) = Proxy::new(conn, WATCHER, WATCHER_PATH, WATCHER_IFACE) {
        if let Ok(items) = proxy.get_property::<Vec<String>>("RegisteredStatusNotifierItems") {
            out.extend(items);
        }
    }
    out
}

fn probe_registrations(conn: &Connection, probed: &mut HashSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let skip = [
        "org.freedesktop.",
        "org.gtk.",
        "org.kde.StatusNotifier",
        "org.mpris.",
        "org.pulseaudio.",
        "org.bluez.",
        "org.gnome.",
        "ca.desrt.",
        "org.a11y.",
        "org.lxde.",
        "com.canonical.AppIndicator",
    ];
    for name in list_names(conn) {
        if name.starts_with(':') || name.starts_with("org.hyprisland") {
            continue;
        }
        if skip.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        if !probed.insert(name.clone()) {
            continue;
        }
        for path in ITEM_PATHS {
            if let Ok(props) =
                Proxy::new(conn, name.as_str(), path, "org.freedesktop.DBus.Properties")
            {
                if props
                    .call::<_, _, OwnedValue>("Get", &(ITEM_IFACE, "Id"))
                    .is_ok()
                {
                    out.push(format!("{name}{path}"));
                    break;
                }
            }
        }
    }
    out
}

fn collect_items(
    conn: &Connection,
    own: &Arc<Mutex<Vec<String>>>,
    probed: &mut HashSet<String>,
) -> Vec<TrayItem> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut items = Vec::new();
    let mut registrations = watched_registrations(conn, own);
    registrations.extend(probe_registrations(conn, probed));

    for service in registrations {
        let Some((name, path)) = split_registration(&service) else {
            continue;
        };
        if !seen.insert((name.clone(), path.clone())) {
            continue;
        }
        if name == WATCHER || name.starts_with("org.freedesktop.DBus") {
            continue;
        }
        if let Some(item) = read_item(conn, &name, &path) {
            items.push(item);
        }
    }
    items
}

fn apply_command(conn: &Connection, items: &HashMap<String, TrayItem>, cmd: TrayCommand) {
    let (ident, method, args) = match cmd {
        TrayCommand::Activate(ident) => (ident, "Activate", (0i32, 0i32)),
        TrayCommand::ContextMenu(ident, x, y) => (ident, "ContextMenu", (x, y)),
    };
    let Some(item) = items.get(&ident) else {
        return;
    };
    let Ok(proxy) = Proxy::new(
        conn,
        item.bus_name.as_str(),
        item.path.as_str(),
        item.iface.as_str(),
    ) else {
        return;
    };
    if proxy.call::<_, _, ()>(method, &args).is_err() && method == "Activate" {
        let _ = proxy.call::<_, _, ()>("SecondaryActivate", &args);
    }
}

pub fn spawn(events: GlibSender<AppEvent>, commands: Receiver<TrayCommand>) {
    thread::spawn(move || {
        let Ok(conn) = Connection::session() else {
            return;
        };

        let own_items = Arc::new(Mutex::new(Vec::new()));
        if !name_has_owner(&conn, WATCHER) {
            let watcher = SniWatcher {
                items: own_items.clone(),
            };
            let _ = conn.object_server().at(WATCHER_PATH, watcher);
            let _ = conn.request_name(WATCHER);
        }

        let mut item_map: HashMap<String, TrayItem> = HashMap::new();
        let mut probed: HashSet<String> = HashSet::new();
        let mut next_poll = std::time::Instant::now();

        loop {
            match commands.recv_timeout(Duration::from_millis(150)) {
                Ok(cmd) => apply_command(&conn, &item_map, cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            if std::time::Instant::now() < next_poll {
                continue;
            }
            next_poll = std::time::Instant::now() + Duration::from_secs(2);

            let items = collect_items(&conn, &own_items, &mut probed);
            item_map = items
                .into_iter()
                .map(|item| (item.snapshot.ident.clone(), item))
                .collect();
            let snapshots: Vec<TraySnapshot> = item_map
                .values()
                .map(|item| item.snapshot.clone())
                .collect();
            if events.send(AppEvent::Tray(snapshots)).is_err() {
                break;
            }
        }
    });
}

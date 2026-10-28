use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use glib::Sender as GlibSender;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::model::{AppEvent, BluezSnapshot};

type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

fn read_bluez() -> BluezSnapshot {
    let Ok(conn) = Connection::system() else {
        return BluezSnapshot::default();
    };
    let Ok(proxy) = Proxy::new(
        &conn,
        "org.bluez",
        "/",
        "org.freedesktop.DBus.ObjectManager",
    ) else {
        return BluezSnapshot::default();
    };

    let objects: ManagedObjects = match proxy.call("GetManagedObjects", &()) {
        Ok(v) => v,
        Err(_) => return BluezSnapshot::default(),
    };

    let mut found: Option<(i32, String, Option<u8>)> = None;
    for ifaces in objects.values() {
        let Some(dev) = ifaces.get("org.bluez.Device1") else {
            continue;
        };
        let connected = dev
            .get("Connected")
            .and_then(|v| bool::try_from(v.clone()).ok())
            .unwrap_or(false);
        if !connected {
            continue;
        }
        let name = dev
            .get("Name")
            .and_then(|v| String::try_from(v.clone()).ok())
            .or_else(|| {
                dev.get("Alias")
                    .and_then(|v| String::try_from(v.clone()).ok())
            })
            .unwrap_or_else(|| "Bluetooth device".to_string());
        let pct = ifaces
            .get("org.bluez.Battery1")
            .and_then(|b| b.get("Percentage"))
            .and_then(|v| u8::try_from(v.clone()).ok());
        let score = if pct.is_some() { 2 } else { 1 };
        if found.as_ref().map(|(s, _, _)| score > *s).unwrap_or(true) {
            found = Some((score, name, pct));
        }
    }

    match found {
        Some((_, name, percentage)) => BluezSnapshot {
            connected: true,
            name,
            percentage,
        },
        None => BluezSnapshot::default(),
    }
}

pub fn spawn(events: GlibSender<AppEvent>) {
    thread::spawn(move || loop {
        let snapshot = read_bluez();
        if events.send(AppEvent::Bluez(snapshot)).is_err() {
            break;
        }
        thread::sleep(Duration::from_secs(15));
    });
}

use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Music,
    System,
    Tray,
}

impl View {
    pub fn index(self) -> usize {
        match self {
            View::Music => 0,
            View::System => 1,
            View::Tray => 2,
        }
    }

    pub fn next(self, direction: i32) -> Self {
        match (self.index() as i32 + direction).rem_euclid(3) {
            0 => View::Music,
            1 => View::System,
            _ => View::Tray,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct MediaSnapshot {
    pub bus_name: Option<String>,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: String,
    pub length: i64,
    pub position: f64,
}

impl MediaSnapshot {
    pub fn has_media(&self) -> bool {
        !self.title.is_empty()
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }
}

#[derive(Clone, Debug)]
pub enum TrayIcon {
    Named(String),
    Raw {
        width: i32,
        height: i32,
        data: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
pub struct TraySnapshot {
    pub ident: String,
    pub title: String,
    pub icon: Option<TrayIcon>,
}

#[derive(Clone, Debug, Default)]
pub struct BluezSnapshot {
    pub connected: bool,
    pub name: String,
    pub percentage: Option<u8>,
}

#[derive(Clone, Debug)]
pub enum AppEvent {
    Media(MediaSnapshot),
    Art { url: String, data: Option<Vec<u8>> },
    Tray(Vec<TraySnapshot>),
    Bluez(BluezSnapshot),
    Audio { levels: [f32; 4], playing: bool },
}

#[derive(Clone, Debug)]
pub enum MediaCommand {
    PlayPause,
    Next,
    Previous,
    Seek(f64),
    NudgeVolume(f64),
}

#[derive(Clone, Debug)]
pub enum TrayCommand {
    Activate(String),
    ContextMenu(String, i32, i32),
}

#[derive(Clone, Default)]
pub struct SharedMedia {
    pub bus_name: Arc<Mutex<Option<String>>>,
    pub pid: Arc<Mutex<Option<u32>>>,
}

impl SharedMedia {
    pub fn set(&self, bus_name: Option<String>, pid: Option<u32>) {
        *self.bus_name.lock().unwrap() = bus_name;
        *self.pid.lock().unwrap() = pid;
    }

    pub fn get(&self) -> (Option<String>, Option<u32>) {
        (
            self.bus_name.lock().unwrap().clone(),
            *self.pid.lock().unwrap(),
        )
    }
}

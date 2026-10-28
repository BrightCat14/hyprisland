use std::collections::HashSet;
use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use glib::Sender as GlibSender;
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use crate::model::{AppEvent, SharedMedia};

const SAMPLE_RATE: usize = 44_100;
const CHUNK: usize = 1024;
const BAND_EDGES_HZ: [f64; 5] = [250.0, 2000.0, 6000.0, 12500.0, 16000.0];
const DB_FLOOR: f32 = -40.0;
const DB_CEIL: f32 = -10.0;
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";

#[derive(Clone, Debug)]
struct StreamNode {
    serial: i64,
    app_name: String,
    binary: String,
    pid: Option<i64>,
    running: bool,
}

struct SpectrumAnalyzer {
    window: Vec<f32>,
    win_norm: f32,
    slices: Vec<(usize, usize)>,
    planner: FftPlanner<f32>,
}

impl SpectrumAnalyzer {
    fn new() -> Self {
        let window: Vec<f32> = (0..CHUNK)
            .map(|n| {
                let t = std::f64::consts::TAU * n as f64 / (CHUNK - 1) as f64;
                (0.5 - 0.5 * t.cos()) as f32
            })
            .collect();
        let win_norm: f32 = window.iter().sum::<f32>() / 2.0;
        let mut slices = Vec::new();
        let bin_hz = SAMPLE_RATE as f64 / CHUNK as f64;
        for pair in BAND_EDGES_HZ.windows(2) {
            let lo = pair[0];
            let hi = pair[1];
            let start = (lo / bin_hz).ceil() as usize;
            let end = (hi / bin_hz).ceil() as usize;
            let end = end.min(CHUNK / 2 + 1);
            if start < end {
                slices.push((start, end));
            }
        }
        Self {
            window,
            win_norm,
            slices,
            planner: FftPlanner::new(),
        }
    }

    fn levels(&mut self, pcm: &[i16]) -> [f32; 4] {
        let mut buffer: Vec<Complex<f32>> = pcm
            .iter()
            .take(CHUNK)
            .zip(self.window.iter())
            .map(|(s, w)| Complex::new(*s as f32 / 32768.0 * *w, 0.0))
            .collect();
        let fft = self.planner.plan_fft_forward(CHUNK);
        fft.process(&mut buffer);

        let mut out = [0.0f32; 4];
        for (i, (start, end)) in self.slices.iter().enumerate().take(4) {
            let mut sum = 0.0f32;
            for bin in &buffer[*start..*end] {
                let mag = bin.norm() / self.win_norm;
                sum += mag * mag;
            }
            let amp = sum.sqrt();
            let db = 20.0 * amp.max(1e-9).log10();
            let level = (db - DB_FLOOR) / (DB_CEIL - DB_FLOOR);
            out[i] = if level.is_nan() {
                0.0
            } else {
                level.clamp(0.0, 1.0)
            };
        }
        out
    }
}

struct Capture {
    child: Child,
    stream: ChildStdout,
}

impl Capture {
    fn new(serial: i64) -> Option<Self> {
        let mut child = Command::new("pw-record")
            .args([
                format!("--target={serial}").as_str(),
                "--format=s16",
                &format!("--rate={SAMPLE_RATE}"),
                "--channels=1",
                "-P",
                "{ node.dont-fallback=true node.dont-reconnect=true }",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stream = child.stdout.take()?;
        let mut capture = Self { child, stream };
        consume_wav_header(&mut capture.stream);
        Some(capture)
    }

    fn close(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn consume_wav_header(stream: &mut ChildStdout) {
    let mut head = [0u8; 12];
    if stream.read_exact(&mut head).is_err() {
        return;
    }
    if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return;
    }
    loop {
        let mut chunk_hdr = [0u8; 8];
        if stream.read_exact(&mut chunk_hdr).is_err() {
            return;
        }
        let size =
            u32::from_le_bytes([chunk_hdr[4], chunk_hdr[5], chunk_hdr[6], chunk_hdr[7]]) as usize;
        if &chunk_hdr[0..4] == b"data" {
            return;
        }
        let skip = size + (size & 1);
        if skip > 65_536 {
            return;
        }
        let mut sink = vec![0u8; skip];
        if stream.read_exact(&mut sink).is_err() {
            return;
        }
    }
}

fn json_i64(value: &serde_json::Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    if let Some(s) = value.as_str() {
        return s.trim().parse().ok();
    }
    if let Some(n) = value.as_u64() {
        return i64::try_from(n).ok();
    }
    None
}

fn list_output_streams() -> Vec<StreamNode> {
    let Ok(output) = Command::new("pw-dump").output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    let Some(objects) = payload.as_array() else {
        return Vec::new();
    };
    let mut streams = Vec::new();
    for obj in objects {
        if obj.get("type").and_then(|v| v.as_str()) != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(info) = obj.get("info") else {
            continue;
        };
        let Some(props) = info.get("props") else {
            continue;
        };
        if props.get("media.class").and_then(|v| v.as_str()) != Some("Stream/Output/Audio") {
            continue;
        }
        let Some(serial) = props.get("object.serial").and_then(json_i64) else {
            continue;
        };
        let binary = props
            .get("application.process.binary")
            .and_then(|v| v.as_str())
            .map(|s| {
                std::path::Path::new(s)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| s.to_string())
            })
            .unwrap_or_default();
        streams.push(StreamNode {
            serial,
            app_name: props
                .get("application.name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            binary,
            pid: props.get("application.process.id").and_then(json_i64),
            running: info.get("state").and_then(|v| v.as_str()) == Some("running"),
        });
    }
    streams
}

fn process_tree(root: i64) -> HashSet<i64> {
    let mut children: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            let Ok(pid) = name.parse::<i64>() else {
                continue;
            };
            let Ok(data) = std::fs::read(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let text = String::from_utf8_lossy(&data);
            let Some((_, rest)) = text.rsplit_once(')') else {
                continue;
            };
            let mut parts = rest.split_whitespace();
            let _state = parts.next();
            let Some(ppid) = parts.next().and_then(|p| p.parse::<i64>().ok()) else {
                continue;
            };
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut seen = HashSet::new();
    seen.insert(root);
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if let Some(kids) = children.get(&pid) {
            for child in kids {
                if seen.insert(*child) {
                    stack.push(*child);
                }
            }
        }
    }
    seen
}

fn best_stream(candidates: &[&StreamNode]) -> Option<StreamNode> {
    candidates
        .iter()
        .max_by_key(|s| (s.running, s.serial))
        .map(|s| (*s).clone())
}

fn find_stream_for_player(
    player_bus: Option<String>,
    player_pid: Option<u32>,
    streams: &[StreamNode],
) -> Option<StreamNode> {
    let bus = player_bus?;
    let pid = player_pid?;
    let pids = process_tree(pid as i64);
    let by_pid: Vec<&StreamNode> = streams
        .iter()
        .filter(|s| s.pid.map(|p| pids.contains(&p)).unwrap_or(false))
        .collect();
    if let Some(stream) = best_stream(&by_pid) {
        return Some(stream);
    }

    let short = bus.strip_prefix(MPRIS_PREFIX).unwrap_or(&bus);
    let key = short.split('.').next().unwrap_or("").to_lowercase();
    if key.is_empty() {
        return None;
    }
    let by_name: Vec<&StreamNode> = streams
        .iter()
        .filter(|s| key == s.binary.to_lowercase() || s.app_name.to_lowercase().contains(&key))
        .collect();
    best_stream(&by_name)
}

pub fn spawn(events: GlibSender<AppEvent>, media: SharedMedia, running: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut analyzer = SpectrumAnalyzer::new();
        let mut capture: Option<Capture> = None;
        let mut target_serial: Option<i64> = None;
        let mut smooth = [0.0f32; 4];
        let mut streams: Vec<StreamNode> = Vec::new();
        let mut last_scan = Instant::now() - Duration::from_secs(10);
        let frame_bytes = CHUNK * 2;
        let mut frame = vec![0u8; frame_bytes];

        while running.load(Ordering::Relaxed) {
            let (bus, pid) = media.get();
            if bus.is_none() || pid.is_none() {
                if let Some(c) = capture.take() {
                    c.close();
                }
                target_serial = None;
                smooth = [0.0; 4];
                let _ = events.send(AppEvent::Audio {
                    levels: smooth,
                    playing: false,
                });
                thread::sleep(Duration::from_millis(800));
                continue;
            }

            if capture.is_none() || last_scan.elapsed() >= Duration::from_secs(2) {
                last_scan = Instant::now();
                streams = list_output_streams();
            }
            let wanted = find_stream_for_player(bus.clone(), pid, &streams).map(|s| s.serial);
            let theme_serial = wanted.or(target_serial);

            if let Some(serial) = theme_serial {
                if target_serial != Some(serial) {
                    if let Some(c) = capture.take() {
                        c.close();
                    }
                    capture = Capture::new(serial);
                    target_serial = Some(serial);
                }
            } else {
                if let Some(c) = capture.take() {
                    c.close();
                }
                target_serial = None;
            }

            let Some(cap) = capture.as_mut() else {
                smooth = [0.0; 4];
                let _ = events.send(AppEvent::Audio {
                    levels: smooth,
                    playing: false,
                });
                thread::sleep(Duration::from_millis(500));
                continue;
            };

            if cap.stream.read_exact(&mut frame).is_err() {
                if let Some(c) = capture.take() {
                    c.close();
                }
                target_serial = None;
                smooth = [0.0; 4];
                let _ = events.send(AppEvent::Audio {
                    levels: smooth,
                    playing: false,
                });
                thread::sleep(Duration::from_millis(500));
                continue;
            }

            let (chunks, _) = frame.as_chunks::<2>();
            let pcm: Vec<i16> = chunks
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            let levels = analyzer.levels(&pcm);
            for i in 0..4 {
                let factor = if levels[i] > smooth[i] { 0.6 } else { 0.12 };
                smooth[i] += factor * (levels[i] - smooth[i]);
                smooth[i] = smooth[i].max(0.0);
            }

            if events
                .send(AppEvent::Audio {
                    levels: smooth,
                    playing: true,
                })
                .is_err()
            {
                if let Some(c) = capture.take() {
                    c.close();
                }
                break;
            }
        }

        if let Some(c) = capture.take() {
            c.close();
        }
    });
}

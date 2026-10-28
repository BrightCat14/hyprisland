use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender as StdSender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cairo::{Context, LinearGradient};
use gdk_pixbuf::{Pixbuf, PixbufLoader};
use gtk::prelude::*;
use gtk_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::audio;
use crate::bluez;
use crate::layout::*;
use crate::media;
use crate::model::*;
use crate::render::*;
use crate::spring::Spring;
use crate::tray;

const SPRING_STIFFNESS: f64 = 330.0;
const SPRING_DAMPING: f64 = 30.0;

enum Hit {
    Circle { x: f64, y: f64, r: f64 },
    Rect { x: f64, y: f64, w: f64, h: f64 },
}

struct TrayItemUi {
    snapshot: TraySnapshot,
    pixbuf: Option<Pixbuf>,
}

pub struct State {
    window: gtk::Window,
    area: gtk::DrawingArea,
    width: Spring,
    height: Spring,
    radius: Spring,
    content: Spring,
    view: View,
    expanded: bool,
    hover: bool,
    expand_until: Option<Instant>,
    press_pos: Option<(f64, f64)>,
    click_consumed: bool,
    hit: HashMap<String, Hit>,
    mouse: (f64, f64),
    geom_dirty: bool,
    media: MediaSnapshot,
    media_received: Instant,
    art: Option<Pixbuf>,
    art_cache: HashMap<String, Pixbuf>,
    tray: Vec<TrayItemUi>,
    bluez: BluezSnapshot,
    audio_levels: [f32; 4],
    audio_playing: bool,
    media_cmd: StdSender<MediaCommand>,
    tray_cmd: StdSender<TrayCommand>,
    started: Instant,
}

fn clock() -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|d| d.format("%H:%M").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn date() -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|d| d.format("%A, %d %B").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn decode_pixbuf(data: &[u8]) -> Option<Pixbuf> {
    let loader = PixbufLoader::new();
    loader.write(data).ok()?;
    loader.close().ok()?;
    let pb = loader.pixbuf()?;
    let w = pb.width();
    let h = pb.height();
    if w <= 0 || h <= 0 {
        return None;
    }
    let side = w.min(h);
    let cropped = pb.new_subpixbuf((w - side) / 2, (h - side) / 2, side, side);
    cropped.scale_simple(320, 320, gdk_pixbuf::InterpType::Bilinear)
}

fn pixbuf_from_raw(width: i32, height: i32, data: &[u8]) -> Option<Pixbuf> {
    let need = (width.max(0) as usize) * (height.max(0) as usize) * 4;
    if need == 0 || data.len() < need {
        return None;
    }
    let bytes = glib::Bytes::from(&data[..need]);
    let pb = Pixbuf::from_bytes(
        &bytes,
        gdk_pixbuf::Colorspace::Rgb,
        true,
        8,
        width,
        height,
        width * 4,
    );
    if width > 64 || height > 64 {
        pb.scale_simple(64, 64, gdk_pixbuf::InterpType::Bilinear)
    } else {
        Some(pb)
    }
}

impl State {
    fn targets(&self) -> (f64, f64, f64, f64) {
        if self.expanded {
            return match self.view {
                View::Music => (EXP_W, EXP_H, 32.0, 1.0),
                View::System => (SYS_EXP_W, SYS_EXP_H, 32.0, 1.0),
                View::Tray => {
                    let n = self.tray.len();
                    let w = if n > 0 { (64 + n * 72).max(500) } else { 500 };
                    ((w as f64).min(WIN_W - 40.0), TRAY_EXP_H, 32.0, 1.0)
                }
            };
        }
        match self.view {
            View::Music => {
                if self.media.has_media() {
                    (MEDIA_W, MEDIA_H, 24.0, 0.0)
                } else {
                    (COMPACT_W, COMPACT_H, COMPACT_H / 2.0, 0.0)
                }
            }
            View::System => (SYS_W, SYS_H, SYS_H / 2.0, 0.0),
            View::Tray => {
                let n = self.tray.len();
                let w = if n > 0 { (56 + n * 38).max(210) } else { 210 };
                ((w as f64).min(WIN_W - 40.0), TRAY_H, TRAY_H / 2.0, 0.0)
            }
        }
    }

    fn refresh(&mut self, initial: bool) {
        let (w, h, r, c) = self.targets();
        if initial {
            self.width.snap(w);
            self.height.snap(h);
            self.radius.snap(r);
            self.content.snap(c);
        } else {
            self.width.set(w);
            self.height.set(h);
            self.radius.set(r);
            self.content.set(c);
        }
        self.update_exclusive_zone();
    }

    fn update_exclusive_zone(&self) {
        self.window.set_layer(Layer::Top);
        self.window.set_exclusive_zone(COMPACT_H as i32 + 8);
    }

    fn expand(&mut self, auto: bool) {
        if self.expanded {
            return;
        }
        self.expanded = true;
        self.refresh(false);
        self.expand_until = if auto {
            Some(Instant::now() + Duration::from_secs(5))
        } else {
            None
        };
    }

    fn collapse(&mut self) {
        if !self.expanded {
            return;
        }
        self.expanded = false;
        self.refresh(false);
        self.expand_until = None;
    }

    fn cycle_view(&mut self, direction: i32) {
        self.view = self.view.next(direction);
        if !self.expanded {
            self.expanded = true;
            self.refresh(false);
            self.expand_until = Some(Instant::now() + Duration::from_millis(3500));
        } else {
            self.refresh(false);
            self.expand_until = Some(Instant::now() + Duration::from_secs(4));
        }
        self.area.queue_draw();
    }

    fn can_expand(&self) -> bool {
        self.view != View::Music || self.media.has_media()
    }

    fn pill_rect(&self) -> (f64, f64, f64, f64) {
        let w = self.width.value;
        let h = self.height.value;
        ((WIN_W - w) / 2.0, 0.0, w, h)
    }

    fn apply_input_region(&self) {
        let Some(gdk_window) = self.window.window() else {
            return;
        };
        let (x, y, w, h) = self.pill_rect();
        let rect =
            cairo::RectangleInt::new(x as i32, y as i32, (w as i32).max(1), (h as i32).max(1));
        let region = cairo::Region::create_rectangle(&rect);
        gdk_window.input_shape_combine_region(&region, 0, 0);
    }

    fn position(&self) -> f64 {
        if self.media.is_playing() {
            self.media.position + self.media_received.elapsed().as_secs_f64()
        } else {
            self.media.position
        }
    }

    fn fmt_seconds(seconds: f64) -> String {
        let seconds = seconds.max(0.0) as i64;
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }

    fn inside(pos: (f64, f64), rect: (f64, f64, f64, f64)) -> bool {
        let (px, py) = pos;
        let (rx, ry, rw, rh) = rect;
        rx <= px && px <= rx + rw && ry <= py && py <= ry + rh
    }

    fn tray_item(&self, ident: &str) -> Option<&TrayItemUi> {
        self.tray.iter().find(|item| item.snapshot.ident == ident)
    }

    fn tray_rects(
        &self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        expanded: bool,
    ) -> Vec<(String, (f64, f64, f64, f64))> {
        let items: Vec<&TrayItemUi> = self
            .tray
            .iter()
            .filter(|item| item.pixbuf.is_some())
            .collect();
        let mut rects = Vec::new();
        if items.is_empty() {
            return rects;
        }
        if expanded {
            let tile = 56.0;
            let step = 72.0;
            let total = items.len() as f64 * step;
            let start = x + (w - total) / 2.0 + (step - tile) / 2.0;
            let ty = y + (h - 78.0) / 2.0;
            for (i, item) in items.iter().enumerate() {
                rects.push((
                    item.snapshot.ident.clone(),
                    (start + i as f64 * step, ty, tile, tile),
                ));
            }
        } else {
            let icon = 26.0;
            let step = 38.0;
            let total = (items.len() as f64 - 1.0) * step + icon;
            let start = x + (w - total) / 2.0;
            let ty = y + (h - icon) / 2.0;
            for (i, item) in items.iter().enumerate() {
                rects.push((
                    item.snapshot.ident.clone(),
                    (start + i as f64 * step, ty, icon, icon),
                ));
            }
        }
        rects
    }

    fn music_geometry(
        &self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> (f64, f64, f64, f64, f64, f64, f64) {
        let art = 150.0;
        let ax = x + 28.0;
        let ay = y + (h - art) / 2.0;
        let tx = ax + art + 26.0;
        let cw = x + w - 28.0 - tx;
        let cy = y + h - 46.0;
        let cxm = tx + cw / 2.0;
        (art, ax, ay, tx, cw, cy, cxm)
    }

    fn update_hit_regions(&mut self, x: f64, y: f64, w: f64, h: f64) {
        if !self.expanded || self.view != View::Music {
            self.hit.clear();
            return;
        }
        let (_, _, _, tx, cw, cy, cxm) = self.music_geometry(x, y, w, h);
        self.hit = HashMap::from([
            (
                "prev".to_string(),
                Hit::Circle {
                    x: cxm - 50.0,
                    y: cy,
                    r: 14.0,
                },
            ),
            (
                "play".to_string(),
                Hit::Circle {
                    x: cxm,
                    y: cy,
                    r: 16.0,
                },
            ),
            (
                "next".to_string(),
                Hit::Circle {
                    x: cxm + 50.0,
                    y: cy,
                    r: 14.0,
                },
            ),
            (
                "progress".to_string(),
                Hit::Rect {
                    x: tx,
                    y: y + 108.0,
                    w: cw,
                    h: 18.0,
                },
            ),
        ]);
    }

    fn hit_at(&self, px: f64, py: f64) -> Option<&str> {
        for (name, hit) in &self.hit {
            if let Hit::Circle { x, y, r } = hit {
                if (px - x).powi(2) + (py - y).powi(2) <= r * r {
                    return Some(name.as_str());
                }
            }
        }
        None
    }

    fn draw(&mut self, cr: &Context) {
        let w = self.width.value;
        let h = self.height.value;
        let x = (WIN_W - w) / 2.0;
        let y = 0.0;
        let r = self.radius.value;
        let t = self.content.value.clamp(0.0, 1.0);

        cr.set_operator(cairo::Operator::Clear);
        let _ = cr.paint();
        cr.set_operator(cairo::Operator::Over);

        self.draw_shadow(cr, x, y, w, h, r);
        self.draw_body(cr, x, y, w, h, r);

        let _ = cr.save();
        rounded_path(cr, x, y, w, h, r);
        cr.clip();

        let compact_a = (1.0 - t / 0.30).max(0.0);
        let expanded_a = ((t - 0.38) / 0.62).max(0.0);
        if compact_a > 0.005 {
            self.draw_compact(cr, x, y, w, h, compact_a);
        }
        if expanded_a > 0.005 {
            self.draw_expanded(cr, x, y, w, h, expanded_a);
        }
        let _ = cr.restore();

        stroke_rounded(cr, x, y, w, h, r, COL_BORDER, 1.0);
    }

    fn draw_shadow(&self, cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
        for i in (1..=10).rev() {
            let i = i as f64;
            let alpha = 0.020 * (1.0 - i / 11.0);
            rounded_path(
                cr,
                x - i * 0.6,
                y + 2.0 + i * 0.9,
                w + i * 1.2,
                h + i * 1.2,
                r + i * 0.6,
            );
            cr.set_source_rgba(0.0, 0.0, 0.0, alpha);
            let _ = cr.fill();
        }
    }

    fn draw_body(&self, cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
        let grad = LinearGradient::new(x, y, x, y + h);
        grad.add_color_stop_rgba(0.0, COL_BG_TOP.0, COL_BG_TOP.1, COL_BG_TOP.2, COL_BG_TOP.3);
        grad.add_color_stop_rgba(1.0, COL_BG_BOT.0, COL_BG_BOT.1, COL_BG_BOT.2, COL_BG_BOT.3);
        rounded_path(cr, x, y, w, h, r);
        cr.set_source(&grad).unwrap();
        let _ = cr.fill();

        let sheen = LinearGradient::new(x, y, x, y + h.min(30.0));
        sheen.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.055);
        sheen.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
        rounded_path(cr, x, y, w, h, r);
        cr.set_source(&sheen).unwrap();
        let _ = cr.fill();
    }

    fn draw_compact(&mut self, cr: &Context, x: f64, y: f64, w: f64, h: f64, alpha: f64) {
        if alpha <= 0.01 {
            return;
        }
        match self.view {
            View::Music => self.compact_music(cr, x, y, w, h, alpha),
            View::System => self.compact_sys(cr, x, y, w, h, alpha),
            View::Tray => self.compact_tray(cr, x, y, w, h, alpha),
        }
    }

    fn draw_expanded(&mut self, cr: &Context, x: f64, y: f64, w: f64, h: f64, alpha: f64) {
        match self.view {
            View::Music => self.expanded_music(cr, x, y, w, h, alpha),
            View::System => self.expanded_sys(cr, x, y, w, h, alpha),
            View::Tray => self.expanded_tray(cr, x, y, w, h, alpha),
        }
        self.draw_mode_dots(cr, x, y, w, alpha);
    }

    fn draw_mode_dots(&self, cr: &Context, x: f64, y: f64, w: f64, alpha: f64) {
        for i in 0..3 {
            let cx = x + w - 26.0 - (2 - i) as f64 * 10.0;
            let active = i == self.view.index();
            cr.arc(cx, y + 18.0, 2.4, 0.0, std::f64::consts::TAU);
            if active {
                cr.set_source_rgba(COL_ACCENT.0, COL_ACCENT.1, COL_ACCENT.2, alpha);
            } else {
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.18 * alpha);
            }
            let _ = cr.fill();
        }
    }

    fn compact_music(&self, cr: &Context, x: f64, y: f64, w: f64, h: f64, alpha: f64) {
        if self.media.has_media() {
            let art = 44.0;
            let ax = x + 9.0;
            let ay = y + (h - art) / 2.0;
            draw_art(cr, self.art.as_ref(), ax, ay, art, 12.0, alpha);

            let tx = ax + art + 12.0;
            let maxw = w - (tx - x) - if self.media.is_playing() { 50.0 } else { 16.0 };
            draw_text(
                cr,
                if self.media.title.is_empty() {
                    "Unknown"
                } else {
                    &self.media.title
                },
                tx,
                y + 10.0,
                13.5,
                (1.0, 1.0, 1.0, 0.95 * alpha),
                pango::Weight::Bold,
                Some(maxw),
                FONT,
                false,
                false,
            );
            draw_text(
                cr,
                if self.media.artist.is_empty() {
                    "Unknown artist"
                } else {
                    &self.media.artist
                },
                tx,
                y + 32.0,
                11.0,
                (1.0, 1.0, 1.0, 0.55 * alpha),
                pango::Weight::Normal,
                Some(maxw),
                FONT,
                false,
                false,
            );

            if self.audio_playing {
                draw_real_eq(
                    cr,
                    x + w - 34.0,
                    y + h / 2.0 - 8.0,
                    16.0,
                    &self.audio_levels,
                    COL_ACCENT,
                    alpha,
                );
            } else {
                draw_eq(
                    cr,
                    x + w - 34.0,
                    y + h / 2.0 - 8.0,
                    16.0,
                    self.started.elapsed().as_secs_f64(),
                    COL_ACCENT,
                    alpha,
                    self.media.is_playing(),
                );
            }
        } else {
            draw_text(
                cr,
                &clock(),
                x + w / 2.0,
                y + (h - 22.0) / 2.0,
                16.0,
                (1.0, 1.0, 1.0, 0.95 * alpha),
                pango::Weight::Bold,
                None,
                CLOCK_FONT,
                false,
                true,
            );
        }
    }

    fn compact_sys(&self, cr: &Context, x: f64, y: f64, w: f64, h: f64, alpha: f64) {
        draw_text(
            cr,
            &clock(),
            x + 53.0,
            y + (h - 24.0) / 2.0,
            17.0,
            (1.0, 1.0, 1.0, 0.95 * alpha),
            pango::Weight::Bold,
            None,
            CLOCK_FONT,
            false,
            true,
        );

        fill_rounded(
            cr,
            x + 106.0,
            y + 14.0,
            1.0,
            h - 28.0,
            0.5,
            (1.0, 1.0, 1.0, 0.10 * alpha),
        );

        draw_headphone(
            cr,
            x + 134.0,
            y + h / 2.0,
            24.0,
            (
                1.0,
                1.0,
                1.0,
                (if self.bluez.connected { 0.85 } else { 0.35 }) * alpha,
            ),
        );

        let tx = x + 156.0;
        let maxw = w - (tx - x) - 18.0;
        let (name, pct) = if self.bluez.connected {
            let pct = self
                .bluez
                .percentage
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "\u{2014}".to_string());
            (self.bluez.name.clone(), pct)
        } else {
            ("No headphones".to_string(), "--".to_string())
        };
        draw_text(
            cr,
            &name,
            tx,
            y + 11.0,
            10.5,
            (1.0, 1.0, 1.0, 0.52 * alpha),
            pango::Weight::Normal,
            Some(maxw),
            FONT,
            false,
            false,
        );
        draw_text(
            cr,
            &pct,
            tx,
            y + 27.0,
            16.0,
            (
                1.0,
                1.0,
                1.0,
                (if self.bluez.connected { 0.95 } else { 0.45 }) * alpha,
            ),
            pango::Weight::Bold,
            Some(maxw),
            FONT,
            false,
            false,
        );
    }

    fn compact_tray(&self, cr: &Context, x: f64, y: f64, w: f64, h: f64, alpha: f64) {
        if self.tray.is_empty() {
            draw_text(
                cr,
                "No tray items",
                x + w / 2.0,
                y + (h - 16.0) / 2.0,
                11.5,
                (1.0, 1.0, 1.0, 0.45 * alpha),
                pango::Weight::Normal,
                None,
                FONT,
                false,
                true,
            );
            return;
        }
        for (ident, rect) in self.tray_rects(x, y, w, h, false) {
            let Some(item) = self.tray_item(&ident) else {
                continue;
            };
            let Some(pb) = item.pixbuf.as_ref() else {
                continue;
            };
            let (ix, iy, iw, ih) = rect;
            let hovered = self.hover && Self::inside(self.mouse, rect);
            if hovered {
                fill_rounded(
                    cr,
                    ix - 5.0,
                    iy - 5.0,
                    iw + 10.0,
                    ih + 10.0,
                    9.0,
                    (1.0, 1.0, 1.0, 0.07 * alpha),
                );
            }
            draw_pixbuf(cr, pb, ix, iy, iw, alpha);
        }
    }

    fn expanded_music(&mut self, cr: &Context, x: f64, y: f64, w: f64, h: f64, t: f64) {
        let (art, ax, ay, tx, cw, cy, cxm) = self.music_geometry(x, y, w, h);
        let has_media = self.media.has_media();
        draw_art(
            cr,
            if has_media { self.art.as_ref() } else { None },
            ax,
            ay,
            art,
            24.0 * t,
            t,
        );

        let (title, artist, album) = if has_media {
            (
                self.media.title.clone(),
                self.media.artist.clone(),
                self.media.album.clone(),
            )
        } else {
            (
                "Nothing playing".to_string(),
                "Start something on your desktop".to_string(),
                String::new(),
            )
        };

        draw_text(
            cr,
            &title,
            tx,
            y + 24.0,
            19.0,
            (1.0, 1.0, 1.0, 0.97 * t),
            pango::Weight::Bold,
            Some(cw),
            FONT,
            false,
            false,
        );
        draw_text(
            cr,
            &artist,
            tx,
            y + 56.0,
            13.0,
            (1.0, 1.0, 1.0, 0.58 * t),
            pango::Weight::Normal,
            Some(cw),
            FONT,
            false,
            false,
        );
        if !album.is_empty() && album != title {
            draw_text(
                cr,
                &album,
                tx,
                y + 79.0,
                11.0,
                (1.0, 1.0, 1.0, 0.34 * t),
                pango::Weight::Normal,
                Some(cw),
                FONT,
                false,
                false,
            );
        }

        let py = y + 112.0;
        let length = if has_media { self.media.length } else { 0 };
        let pos = if has_media { self.position() } else { 0.0 };
        let frac = if length > 0 {
            (pos / length as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let bar_h = 6.0;
        let bar_r = bar_h / 2.0;
        fill_rounded(
            cr,
            tx,
            py + (10.0 - bar_h) / 2.0,
            cw,
            bar_h,
            bar_r,
            (1.0, 1.0, 1.0, 0.16 * t),
        );
        if frac > 0.0 {
            fill_rounded(
                cr,
                tx,
                py + (10.0 - bar_h) / 2.0,
                (cw * frac).max(bar_h),
                bar_h,
                bar_r,
                (COL_ACCENT.0, COL_ACCENT.1, COL_ACCENT.2, t),
            );
            cr.arc(tx + cw * frac, py + 5.0, 4.0, 0.0, std::f64::consts::TAU);
            cr.set_source_rgba(1.0, 1.0, 1.0, t);
            let _ = cr.fill();
        }

        draw_text(
            cr,
            &Self::fmt_seconds(pos),
            tx,
            py + 12.0,
            10.5,
            (1.0, 1.0, 1.0, 0.45 * t),
            pango::Weight::Normal,
            None,
            FONT,
            false,
            false,
        );
        draw_text(
            cr,
            &Self::fmt_seconds(length as f64),
            tx + cw,
            py + 12.0,
            10.5,
            (1.0, 1.0, 1.0, 0.45 * t),
            pango::Weight::Normal,
            None,
            FONT,
            true,
            false,
        );

        self.update_hit_regions(x, y, w, h);
        let (mx, my) = self.mouse;
        for (name, bx) in [("prev", cxm - 50.0), ("play", cxm), ("next", cxm + 50.0)] {
            let (hx, hy, hr) = match self.hit.get(name) {
                Some(Hit::Circle { x, y, r }) => (*x, *y, *r),
                _ => (bx, cy, 0.0),
            };
            let hovered = self.hover && (mx - hx).powi(2) + (my - hy).powi(2) <= hr * hr;
            if name == "play" {
                fill_rounded(
                    cr,
                    bx - 18.0,
                    cy - 18.0,
                    36.0,
                    36.0,
                    18.0,
                    (1.0, 1.0, 1.0, if hovered { 0.17 } else { 0.11 } * t),
                );
            } else if hovered {
                fill_rounded(
                    cr,
                    bx - 16.0,
                    cy - 16.0,
                    32.0,
                    32.0,
                    16.0,
                    (1.0, 1.0, 1.0, 0.07 * t),
                );
            }
        }

        let col = (1.0, 1.0, 1.0, 0.92 * t);
        draw_skip_icon(cr, cxm - 50.0, cy, 16.0, -1.0, col);
        draw_play_icon(cr, cxm, cy, 14.0, self.media.is_playing(), col);
        draw_skip_icon(cr, cxm + 50.0, cy, 16.0, 1.0, col);
    }

    fn expanded_sys(&self, cr: &Context, x: f64, y: f64, w: f64, h: f64, t: f64) {
        draw_text(
            cr,
            &clock(),
            x + 36.0,
            y + 28.0,
            48.0,
            (1.0, 1.0, 1.0, 0.97 * t),
            pango::Weight::Bold,
            None,
            CLOCK_FONT,
            false,
            false,
        );
        draw_text(
            cr,
            &date(),
            x + 38.0,
            y + 104.0,
            12.0,
            (1.0, 1.0, 1.0, 0.50 * t),
            pango::Weight::Normal,
            None,
            FONT,
            false,
            false,
        );
        draw_text(
            cr,
            "Right-click to switch view",
            x + 38.0,
            y + h - 30.0,
            10.0,
            (1.0, 1.0, 1.0, 0.28 * t),
            pango::Weight::Normal,
            None,
            FONT,
            false,
            false,
        );

        fill_rounded(
            cr,
            x + 226.0,
            y + 30.0,
            1.0,
            h - 60.0,
            0.5,
            (1.0, 1.0, 1.0, 0.09 * t),
        );

        let rx = x + 250.0;
        draw_headphone(
            cr,
            rx + 19.0,
            y + 50.0,
            38.0,
            (
                1.0,
                1.0,
                1.0,
                (if self.bluez.connected { 0.9 } else { 0.35 }) * t,
            ),
        );

        let (name, sub) = if self.bluez.connected {
            (self.bluez.name.clone(), "Bluetooth audio")
        } else {
            ("No headphones".to_string(), "Connect a Bluetooth device")
        };
        let maxw = x + w - 30.0 - (rx + 50.0);
        draw_text(
            cr,
            &name,
            rx + 50.0,
            y + 32.0,
            15.0,
            (1.0, 1.0, 1.0, 0.95 * t),
            pango::Weight::Bold,
            Some(maxw),
            FONT,
            false,
            false,
        );
        draw_text(
            cr,
            sub,
            rx + 50.0,
            y + 55.0,
            11.0,
            (1.0, 1.0, 1.0, 0.46 * t),
            pango::Weight::Normal,
            Some(maxw),
            FONT,
            false,
            false,
        );

        if let Some(pct) = self.bluez.percentage {
            draw_text(
                cr,
                &format!("{pct}%"),
                rx,
                y + 78.0,
                32.0,
                (1.0, 1.0, 1.0, 0.97 * t),
                pango::Weight::Bold,
                None,
                FONT,
                false,
                false,
            );
            let bar_x = rx;
            let bar_w = x + w - 30.0 - bar_x;
            fill_rounded(
                cr,
                bar_x,
                y + 132.0,
                bar_w,
                8.0,
                4.0,
                (1.0, 1.0, 1.0, 0.14 * t),
            );
            fill_rounded(
                cr,
                bar_x,
                y + 132.0,
                (bar_w * pct as f64 / 100.0).max(8.0),
                8.0,
                4.0,
                (COL_ACCENT.0, COL_ACCENT.1, COL_ACCENT.2, t),
            );
        } else if self.bluez.connected {
            draw_text(
                cr,
                "Battery unknown",
                rx,
                y + 86.0,
                13.0,
                (1.0, 1.0, 1.0, 0.45 * t),
                pango::Weight::Normal,
                None,
                FONT,
                false,
                false,
            );
        } else {
            draw_text(
                cr,
                "--",
                rx,
                y + 78.0,
                32.0,
                (1.0, 1.0, 1.0, 0.40 * t),
                pango::Weight::Bold,
                None,
                FONT,
                false,
                false,
            );
        }
    }

    fn expanded_tray(&self, cr: &Context, x: f64, y: f64, w: f64, h: f64, t: f64) {
        draw_text(
            cr,
            "System tray",
            x + 30.0,
            y + 18.0,
            11.0,
            (1.0, 1.0, 1.0, 0.40 * t),
            pango::Weight::Normal,
            None,
            FONT,
            false,
            false,
        );
        if self.tray.is_empty() {
            draw_text(
                cr,
                "No tray items",
                x + w / 2.0,
                y + h / 2.0 - 8.0,
                13.0,
                (1.0, 1.0, 1.0, 0.45 * t),
                pango::Weight::Normal,
                None,
                FONT,
                false,
                true,
            );
            return;
        }

        for (ident, rect) in self.tray_rects(x, y, w, h, true) {
            let Some(item) = self.tray_item(&ident) else {
                continue;
            };
            let Some(pb) = item.pixbuf.as_ref() else {
                continue;
            };
            let (ix, iy, iw, ih) = rect;
            let hovered = self.hover && Self::inside(self.mouse, rect);
            fill_rounded(
                cr,
                ix,
                iy,
                iw,
                ih,
                15.0,
                (1.0, 1.0, 1.0, if hovered { 0.11 } else { 0.05 } * t),
            );
            draw_pixbuf(
                cr,
                pb,
                ix + (iw - 34.0) / 2.0,
                iy + (ih - 34.0) / 2.0,
                34.0,
                t,
            );
            draw_text(
                cr,
                if item.snapshot.title.is_empty() {
                    "app"
                } else {
                    &item.snapshot.title
                },
                ix + iw / 2.0,
                iy + ih + 8.0,
                9.5,
                (1.0, 1.0, 1.0, if hovered { 0.62 } else { 0.42 } * t),
                pango::Weight::Normal,
                Some(iw + 10.0),
                FONT,
                false,
                true,
            );
        }
    }

    fn on_media_change(&mut self) {
        let has = self.media.has_media() || self.media.is_playing();
        if self.view == View::Music {
            if self.expanded {
                if !has {
                    self.collapse();
                }
            } else {
                self.refresh(false);
            }
        }
        self.area.queue_draw();
    }

    fn on_tray_change(&mut self) {
        if self.view == View::Tray {
            self.refresh(false);
        }
        self.area.queue_draw();
    }

    fn apply_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Media(snapshot) => {
                let signature_changed = snapshot.bus_name != self.media.bus_name
                    || snapshot.title != self.media.title
                    || snapshot.artist != self.media.artist
                    || snapshot.playing != self.media.playing
                    || snapshot.art_url != self.media.art_url
                    || snapshot.has_media() != self.media.has_media();
                let art_url = snapshot.art_url.clone();
                self.media = snapshot;
                self.media_received = Instant::now();
                self.art = self.art_cache.get(&art_url).cloned();
                if signature_changed {
                    self.on_media_change();
                } else {
                    self.area.queue_draw();
                }
            }
            AppEvent::Art { url, data } => {
                if let Some(data) = data {
                    if let Some(pb) = decode_pixbuf(&data) {
                        self.art_cache.insert(url.clone(), pb);
                    }
                }
                if self.media.art_url == url {
                    self.art = self.art_cache.get(&url).cloned();
                    self.area.queue_draw();
                }
            }
            AppEvent::Tray(items) => {
                let theme = gtk::IconTheme::for_screen(
                    &gtk::prelude::WidgetExt::screen(&self.window).unwrap(),
                )
                .unwrap_or_default();
                self.tray = items
                    .into_iter()
                    .map(|snapshot| {
                        let pixbuf = match &snapshot.icon {
                            Some(TrayIcon::Named(name)) => theme
                                .load_icon(name, 64, gtk::IconLookupFlags::empty())
                                .ok()
                                .flatten(),
                            Some(TrayIcon::Raw {
                                width,
                                height,
                                data,
                            }) => pixbuf_from_raw(*width, *height, data),
                            None => None,
                        };
                        TrayItemUi { snapshot, pixbuf }
                    })
                    .collect();
                self.on_tray_change();
            }
            AppEvent::Bluez(snapshot) => {
                self.bluez = snapshot;
                self.area.queue_draw();
            }
            AppEvent::Audio { levels, playing } => {
                self.audio_levels = levels;
                self.audio_playing = playing;
                if playing {
                    self.area.queue_draw();
                }
            }
        }
    }

    fn tick(&mut self) {
        let mut moving = false;
        if self.width.update(1.0 / 60.0) {
            moving = true;
        }
        if self.height.update(1.0 / 60.0) {
            moving = true;
        }
        if self.radius.update(1.0 / 60.0) {
            moving = true;
        }
        if self.content.update(1.0 / 60.0) {
            moving = true;
        }

        if !moving && !self.geom_dirty {
            if let Some(until) = self.expand_until {
                if self.expanded && !self.hover && Instant::now() > until {
                    self.collapse();
                }
            }
        }
        if moving {
            self.geom_dirty = true;
        }
        if self.geom_dirty {
            self.apply_input_region();
            self.update_exclusive_zone();
            self.area.queue_draw();
            self.geom_dirty = false;
        } else if self.view == View::Music && self.media.is_playing() {
            self.area.queue_draw();
        }
    }

    fn on_press(&mut self, event: &gdk::EventButton) {
        let (ex, ey) = event.position();
        if event.button() == 3 {
            self.press_pos = None;
            let (x, y, w, h) = self.pill_rect();
            if self.view == View::Tray {
                for (ident, rect) in self.tray_rects(x, y, w, h, self.expanded) {
                    if Self::inside((ex, ey), rect) {
                        let (rx, ry) = event.root();
                        let _ = self
                            .tray_cmd
                            .send(TrayCommand::ContextMenu(ident, rx as i32, ry as i32));
                        return;
                    }
                }
            }
            let direction = if ex < x + w / 2.0 { -1 } else { 1 };
            self.cycle_view(direction);
            return;
        }

        let (x, y, w, h) = self.pill_rect();
        self.press_pos = Some((ex, ey));
        self.click_consumed = false;

        if self.view == View::Tray {
            for (ident, rect) in self.tray_rects(x, y, w, h, self.expanded) {
                if Self::inside((ex, ey), rect) {
                    let _ = self.tray_cmd.send(TrayCommand::Activate(ident));
                    self.click_consumed = true;
                    return;
                }
            }
        }

        if self.expanded {
            self.update_hit_regions(x, y, w, h);
            if self.view == View::Music && self.media.has_media() {
                if let Some(Hit::Rect {
                    x: px,
                    y: py,
                    w: pw,
                    h: ph,
                }) = self.hit.get("progress")
                {
                    if *px <= ex && ex <= *px + *pw && *py <= ey && ey <= *py + *ph {
                        let frac = if *pw > 0.0 {
                            ((ex - *px) / *pw).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        let _ = self
                            .media_cmd
                            .send(MediaCommand::Seek(frac * self.media.length as f64));
                        self.click_consumed = true;
                        self.expand_until = None;
                        return;
                    }
                }
                match self.hit_at(ex, ey) {
                    Some("play") => {
                        let _ = self.media_cmd.send(MediaCommand::PlayPause);
                        self.click_consumed = true;
                        return;
                    }
                    Some("prev") => {
                        let _ = self.media_cmd.send(MediaCommand::Previous);
                        self.click_consumed = true;
                        return;
                    }
                    Some("next") => {
                        let _ = self.media_cmd.send(MediaCommand::Next);
                        self.click_consumed = true;
                        return;
                    }
                    _ => {}
                }
            }
            self.expand_until = None;
        }
    }

    fn on_release(&mut self, event: &gdk::EventButton) {
        let Some((sx, sy)) = self.press_pos else {
            return;
        };
        let (ex, ey) = event.position();
        let dx = ex - sx;
        let dy = ey - sy;
        let moved = (dx * dx + dy * dy).sqrt() > 6.0;

        if self.expanded {
            if moved && dy < -40.0 && dy.abs() > dx.abs() {
                self.collapse();
            } else if moved
                && dx.abs() > 70.0
                && dx.abs() > dy.abs() * 1.5
                && self.view == View::Music
            {
                if dx > 0.0 {
                    let _ = self.media_cmd.send(MediaCommand::Previous);
                } else {
                    let _ = self.media_cmd.send(MediaCommand::Next);
                }
            } else if !moved && !self.click_consumed {
                self.collapse();
            }
        } else if !self.click_consumed && self.can_expand() && (!moved || dy > 30.0) {
            self.expand(false);
        }

        self.press_pos = None;
    }

    fn on_motion(&mut self, event: &gdk::EventMotion) {
        let (ex, ey) = event.position();
        self.mouse = (ex, ey);
        let Some((sx, sy)) = self.press_pos else {
            if self.expanded || self.view == View::Tray {
                self.area.queue_draw();
            }
            return;
        };
        let dx = ex - sx;
        let dy = ey - sy;
        if !self.expanded && self.can_expand() && dy > 34.0 && dy.abs() > dx.abs() {
            self.expand(false);
        }
    }

    fn on_enter(&mut self, event: &gdk::EventCrossing) {
        self.hover = true;
        self.mouse = event.position();
        self.area.queue_draw();
    }

    fn on_leave(&mut self) {
        self.hover = false;
        if self.expanded {
            self.expand_until = Some(Instant::now() + Duration::from_millis(500));
        }
        self.area.queue_draw();
    }

    fn on_scroll(&mut self, event: &gdk::EventScroll) {
        if self.view != View::Music {
            return;
        }
        match event.direction() {
            gdk::ScrollDirection::Smooth => {
                let (_, dy) = event.delta();
                if dy < 0.0 {
                    let _ = self.media_cmd.send(MediaCommand::NudgeVolume(0.05));
                } else if dy > 0.0 {
                    let _ = self.media_cmd.send(MediaCommand::NudgeVolume(-0.05));
                } else {
                    return;
                }
            }
            gdk::ScrollDirection::Up => {
                let _ = self.media_cmd.send(MediaCommand::NudgeVolume(0.05));
            }
            gdk::ScrollDirection::Down => {
                let _ = self.media_cmd.send(MediaCommand::NudgeVolume(-0.05));
            }
            _ => return,
        }
        self.area.queue_draw();
    }

    fn emit_redraw(&self) {
        self.area.queue_draw();
    }
}

#[allow(deprecated)]
pub fn build() -> gtk::Window {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_decorated(false);
    window.set_resizable(false);
    window.set_skip_taskbar_hint(true);
    window.set_skip_pager_hint(true);
    window.set_keep_above(true);
    window.set_app_paintable(true);
    if let Some(screen) = gtk::prelude::WidgetExt::screen(&window) {
        if let Some(visual) = screen.rgba_visual() {
            window.set_visual(Some(&visual));
        }
    }

    window.init_layer_shell();
    window.set_layer(Layer::Top);
    window.set_anchor(Edge::Top, true);
    window.set_layer_shell_margin(Edge::Top, PILL_TOP + 4);
    window.set_exclusive_zone(COMPACT_H as i32 + 8);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_namespace("dynamic-island");
    window.set_size_request(WIN_W as i32, WIN_H as i32);

    let area = gtk::DrawingArea::new();
    area.set_size_request(WIN_W as i32, WIN_H as i32);
    area.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::POINTER_MOTION_MASK
            | gdk::EventMask::ENTER_NOTIFY_MASK
            | gdk::EventMask::LEAVE_NOTIFY_MASK
            | gdk::EventMask::SCROLL_MASK,
    );
    window.add(&area);

    let (event_tx, event_rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let (media_tx, media_rx) = std::sync::mpsc::channel::<MediaCommand>();
    let (tray_tx, tray_rx) = std::sync::mpsc::channel::<TrayCommand>();

    let media_shared = media::spawn(event_tx.clone(), media_rx);
    tray::spawn(event_tx.clone(), tray_rx);
    bluez::spawn(event_tx.clone());
    let running = Arc::new(AtomicBool::new(true));
    audio::spawn(event_tx.clone(), media_shared, running.clone());

    let state = Rc::new(RefCell::new(State {
        window: window.clone(),
        area: area.clone(),
        width: Spring::new(COMPACT_W, SPRING_STIFFNESS, SPRING_DAMPING),
        height: Spring::new(COMPACT_H, SPRING_STIFFNESS, SPRING_DAMPING),
        radius: Spring::new(COMPACT_H / 2.0, SPRING_STIFFNESS, SPRING_DAMPING),
        content: Spring::new(0.0, 260.0, SPRING_DAMPING),
        view: View::Music,
        expanded: false,
        hover: false,
        expand_until: None,
        press_pos: None,
        click_consumed: false,
        hit: HashMap::new(),
        mouse: (-1000.0, -1000.0),
        geom_dirty: true,
        media: MediaSnapshot::default(),
        media_received: Instant::now(),
        art: None,
        art_cache: HashMap::new(),
        tray: Vec::new(),
        bluez: BluezSnapshot::default(),
        audio_levels: [0.0; 4],
        audio_playing: false,
        media_cmd: media_tx,
        tray_cmd: tray_tx,
        started: Instant::now(),
    }));

    {
        let mut st = state.borrow_mut();
        st.refresh(true);
    }

    let draw_state = state.clone();
    area.connect_draw(move |_, cr| {
        draw_state.borrow_mut().draw(cr);
        glib::Propagation::Proceed
    });

    let press_state = state.clone();
    area.connect_button_press_event(move |_, event| {
        press_state.borrow_mut().on_press(event);
        glib::Propagation::Stop
    });

    let release_state = state.clone();
    area.connect_button_release_event(move |_, event| {
        release_state.borrow_mut().on_release(event);
        glib::Propagation::Stop
    });

    let motion_state = state.clone();
    area.connect_motion_notify_event(move |_, event| {
        motion_state.borrow_mut().on_motion(event);
        glib::Propagation::Proceed
    });

    let enter_state = state.clone();
    area.connect_enter_notify_event(move |_, event| {
        enter_state.borrow_mut().on_enter(event);
        glib::Propagation::Proceed
    });

    let leave_state = state.clone();
    area.connect_leave_notify_event(move |_, _| {
        leave_state.borrow_mut().on_leave();
        glib::Propagation::Proceed
    });

    let scroll_state = state.clone();
    area.connect_scroll_event(move |_, event| {
        scroll_state.borrow_mut().on_scroll(event);
        glib::Propagation::Stop
    });

    let event_state = state.clone();
    event_rx.attach(None, move |event| {
        event_state.borrow_mut().apply_event(event);
        glib::ControlFlow::Continue
    });

    let tick_state = state.clone();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        tick_state.borrow_mut().tick();
        glib::ControlFlow::Continue
    });

    let destroy_running = running.clone();
    window.connect_destroy(move |_| {
        destroy_running.store(false, Ordering::Relaxed);
    });

    window.show_all();
    if let Ok(st) = state.try_borrow() {
        st.emit_redraw();
        st.apply_input_region();
    }

    window
}

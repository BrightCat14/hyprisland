use cairo::{Context, LinearGradient};
use gdk::prelude::GdkContextExt;
use gdk_pixbuf::Pixbuf;
use pangocairo::functions::{create_layout, show_layout};

use crate::layout::{Rgba, FONT};

pub fn rounded_path(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.max(0.0).min(w / 2.0).min(h / 2.0);
    let pi = std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -pi / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, pi / 2.0);
    cr.arc(x + r, y + h - r, r, pi / 2.0, pi);
    cr.arc(x + r, y + r, r, pi, 3.0 * pi / 2.0);
    cr.close_path();
}

pub fn fill_rounded(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64, rgba: Rgba) {
    rounded_path(cr, x, y, w, h, r);
    cr.set_source_rgba(rgba.0, rgba.1, rgba.2, rgba.3);
    let _ = cr.fill();
}

#[allow(clippy::too_many_arguments)]
pub fn stroke_rounded(
    cr: &Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    r: f64,
    rgba: Rgba,
    width: f64,
) {
    rounded_path(cr, x + 0.5, y + 0.5, w - 1.0, h - 1.0, r);
    cr.set_source_rgba(rgba.0, rgba.1, rgba.2, rgba.3);
    cr.set_line_width(width);
    let _ = cr.stroke();
}

#[allow(clippy::too_many_arguments)]
pub fn draw_text(
    cr: &Context,
    text: &str,
    x: f64,
    y: f64,
    size: f64,
    color: Rgba,
    weight: pango::Weight,
    maxw: Option<f64>,
    family: &str,
    align_right: bool,
    align_center: bool,
) -> (f64, f64) {
    let layout = create_layout(cr);
    let mut desc = pango::FontDescription::new();
    desc.set_family(family);
    desc.set_size((size * pango::SCALE as f64) as i32);
    desc.set_weight(weight);
    layout.set_font_description(Some(&desc));
    if let Some(maxw) = maxw {
        layout.set_width((maxw * pango::SCALE as f64) as i32);
        layout.set_ellipsize(pango::EllipsizeMode::End);
    } else {
        layout.set_width(-1);
    }
    layout.set_text(text);
    let (_, logical) = layout.extents();
    let w = logical.width() as f64 / pango::SCALE as f64;
    let h = logical.height() as f64 / pango::SCALE as f64;
    let draw_x = if align_right {
        x - w
    } else if align_center {
        x - w / 2.0
    } else {
        x
    };
    cr.set_source_rgba(color.0, color.1, color.2, color.3);
    cr.move_to(draw_x, y);
    show_layout(cr, &layout);
    (w, h)
}

pub fn draw_pixbuf(cr: &Context, pb: &Pixbuf, x: f64, y: f64, size: f64, opacity: f64) {
    let pw = pb.width() as f64;
    let ph = pb.height() as f64;
    if pw <= 0.0 || ph <= 0.0 {
        return;
    }
    let _ = cr.save();
    cr.translate(x, y);
    cr.scale(size / pw, size / ph);
    cr.set_source_pixbuf(pb, 0.0, 0.0);
    let _ = cr.paint_with_alpha(opacity);
    let _ = cr.restore();
}

pub fn draw_art(
    cr: &Context,
    pixbuf: Option<&Pixbuf>,
    x: f64,
    y: f64,
    size: f64,
    radius: f64,
    opacity: f64,
) {
    let _ = cr.save();
    rounded_path(cr, x, y, size, size, radius);
    cr.clip();
    match pixbuf {
        Some(pb) => draw_pixbuf(cr, pb, x, y, size, opacity),
        None => {
            let grad = LinearGradient::new(x, y, x + size, y + size);
            grad.add_color_stop_rgba(0.0, 0.22, 0.24, 0.30, opacity);
            grad.add_color_stop_rgba(1.0, 0.10, 0.11, 0.15, opacity);
            cr.set_source(&grad).unwrap();
            let _ = cr.paint();
            draw_text(
                cr,
                "\u{266a}",
                x + size / 2.0,
                y + size / 2.0 - size * 0.22,
                size * 0.34,
                (1.0, 1.0, 1.0, 0.5 * opacity),
                pango::Weight::Normal,
                None,
                FONT,
                false,
                true,
            );
        }
    }
    let _ = cr.restore();
    stroke_rounded(
        cr,
        x,
        y,
        size,
        size,
        radius,
        (1.0, 1.0, 1.0, 0.10 * opacity),
        1.0,
    );
}

pub fn draw_real_eq(
    cr: &Context,
    x: f64,
    y: f64,
    h: f64,
    levels: &[f32; 4],
    color: Rgba,
    opacity: f64,
) {
    let widths = 3.0;
    let gap = 3.0;
    for (i, level) in levels.iter().enumerate().take(4) {
        let frac = (f64::from(*level) * 1.2).clamp(0.05, 1.0);
        let bh = (h * frac).max(2.0);
        let bx = x + i as f64 * (widths + gap);
        let by = y + (h - bh);
        fill_rounded(
            cr,
            bx,
            by,
            widths,
            bh,
            widths / 2.0,
            (color.0, color.1, color.2, color.3 * opacity),
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub fn draw_eq(
    cr: &Context,
    x: f64,
    y: f64,
    h: f64,
    t: f64,
    color: Rgba,
    opacity: f64,
    playing: bool,
) {
    let widths = 3.0;
    let gap = 3.0;
    let phases = [0.0, 0.9, 1.8, 0.4];
    for (i, phase) in phases.iter().enumerate() {
        let frac = if playing {
            0.30 + 0.70 * (0.5 + 0.5 * (t * 3.4 + phase).sin())
        } else {
            0.15
        };
        let bh = (h * frac).max(2.0);
        let bx = x + i as f64 * (widths + gap);
        let by = y + (h - bh);
        fill_rounded(
            cr,
            bx,
            by,
            widths,
            bh,
            widths / 2.0,
            (color.0, color.1, color.2, color.3 * opacity),
        );
    }
}

pub fn draw_play_icon(cr: &Context, cx: f64, cy: f64, s: f64, playing: bool, color: Rgba) {
    if playing {
        let bw = s * 0.22;
        let bh = s;
        for dx in [-s * 0.22, s * 0.22] {
            fill_rounded(
                cr,
                cx + dx - bw / 2.0,
                cy - bh / 2.0,
                bw,
                bh,
                bw / 2.0,
                color,
            );
        }
    } else {
        cr.set_source_rgba(color.0, color.1, color.2, color.3);
        cr.move_to(cx - s * 0.28, cy - s * 0.5);
        cr.line_to(cx - s * 0.28, cy + s * 0.5);
        cr.line_to(cx + s * 0.48, cy);
        cr.close_path();
        let _ = cr.fill();
    }
}

pub fn draw_skip_icon(cr: &Context, cx: f64, cy: f64, s: f64, direction: f64, color: Rgba) {
    let d = direction;
    let base_x = cx - d * s * 0.30;
    let apex_x = cx + d * s * 0.45;
    cr.set_source_rgba(color.0, color.1, color.2, color.3);
    cr.move_to(base_x, cy - s * 0.5);
    cr.line_to(base_x, cy + s * 0.5);
    cr.line_to(apex_x, cy);
    cr.close_path();
    let _ = cr.fill();
    let bar_x = cx + d * s * 0.62;
    fill_rounded(cr, bar_x - 1.4, cy - s * 0.5, 2.8, s, 1.4, color);
}

pub fn draw_headphone(cr: &Context, cx: f64, cy: f64, s: f64, color: Rgba) {
    let _ = cr.save();
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_width(s * 0.11);
    cr.set_source_rgba(color.0, color.1, color.2, color.3);
    let pi = std::f64::consts::PI;
    cr.arc(cx, cy + s * 0.05, s * 0.40, pi, 2.0 * pi);
    let _ = cr.stroke();
    fill_rounded(
        cr,
        cx - s * 0.50,
        cy + s * 0.02,
        s * 0.17,
        s * 0.40,
        s * 0.085,
        color,
    );
    fill_rounded(
        cr,
        cx + s * 0.33,
        cy + s * 0.02,
        s * 0.17,
        s * 0.40,
        s * 0.085,
        color,
    );
    let _ = cr.restore();
}

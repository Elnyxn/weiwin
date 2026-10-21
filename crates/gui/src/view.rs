//! Antialiased Fluent decoration; native child controls retain input and focus.
#![allow(unsafe_op_in_unsafe_fn)]
use std::{mem, ptr};
use windows_sys::Win32::{
    Foundation::RECT,
    Graphics::{Gdi::HDC, GdiPlus as gp},
};

pub const BACKGROUND: u32 = 0x00fffcfa;
pub const WIDTH: i32 = 400;
pub const HEADER: i32 = 42;
pub const HEIGHT: i32 = 530;
pub fn body_y(y: i32) -> i32 {
    y
}
const GREEN: u32 = 0xff08ad70;
const LINE: u32 = 0xffe5eaf0;
const MUTED: u32 = 0xff657388;

pub struct Renderer(usize);
impl Renderer {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let mut input: gp::GdiplusStartupInput = mem::zeroed();
            input.GdiplusVersion = 1;
            let mut token = 0;
            if gp::GdiplusStartup(&mut token, &input, ptr::null_mut()) != 0 {
                return Err("无法初始化界面绘图".into());
            }
            Ok(Self(token))
        }
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe { gp::GdiplusShutdown(self.0) };
    }
}

struct Canvas(*mut gp::GpGraphics);
impl Canvas {
    unsafe fn polygon(&self, points: &[(f32, f32)], color: u32, outline: bool) {
        let points = points
            .iter()
            .map(|&(x, y)| gp::PointF { X: x, Y: y })
            .collect::<Vec<_>>();
        let mut path = ptr::null_mut();
        if gp::GdipCreatePath(gp::FillModeAlternate, &mut path) != 0 {
            return;
        }
        gp::GdipAddPathPolygon(path, points.as_ptr(), points.len() as i32);
        if outline {
            let mut pen = ptr::null_mut();
            if gp::GdipCreatePen1(color, 1.5, gp::UnitPixel, &mut pen) == 0 {
                gp::GdipSetPenLineJoin(pen, gp::LineJoinRound);
                gp::GdipDrawPath(self.0, pen, path);
                gp::GdipDeletePen(pen);
            }
        } else {
            let mut brush = ptr::null_mut();
            if gp::GdipCreateSolidFill(color, &mut brush) == 0 {
                gp::GdipFillPath(self.0, brush.cast(), path);
                gp::GdipDeleteBrush(brush.cast());
            }
        }
        gp::GdipDeletePath(path);
    }
    unsafe fn new(dc: HDC, dpi: u32) -> Option<Self> {
        let mut graphics = ptr::null_mut();
        if gp::GdipCreateFromHDC(dc, &mut graphics) != 0 {
            return None;
        }
        gp::GdipSetSmoothingMode(graphics, gp::SmoothingModeAntiAlias8x8);
        gp::GdipScaleWorldTransform(
            graphics,
            dpi as f32 / 96.,
            dpi as f32 / 96.,
            gp::MatrixOrderPrepend,
        );
        Some(Self(graphics))
    }
    unsafe fn ellipse(&self, x: f32, y: f32, w: f32, h: f32, color: u32) {
        let mut brush = ptr::null_mut();
        if gp::GdipCreateSolidFill(color, &mut brush) == 0 {
            gp::GdipFillEllipse(self.0, brush.cast(), x, y, w, h);
            gp::GdipDeleteBrush(brush.cast());
        }
    }
    unsafe fn rounded(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        color: u32,
        outline: bool,
    ) {
        let mut path = ptr::null_mut();
        if gp::GdipCreatePath(gp::FillModeAlternate, &mut path) != 0 {
            return;
        }
        let d = radius * 2.;
        for (left, top, angle) in [
            (x, y, 180.),
            (x + w - d, y, 270.),
            (x + w - d, y + h - d, 0.),
            (x, y + h - d, 90.),
        ] {
            gp::GdipAddPathArc(path, left, top, d, d, angle, 90.);
        }
        gp::GdipClosePathFigure(path);
        if outline {
            let mut pen = ptr::null_mut();
            if gp::GdipCreatePen1(color, 1., gp::UnitPixel, &mut pen) == 0 {
                gp::GdipDrawPath(self.0, pen, path);
                gp::GdipDeletePen(pen);
            }
        } else {
            let mut brush = ptr::null_mut();
            if gp::GdipCreateSolidFill(color, &mut brush) == 0 {
                gp::GdipFillPath(self.0, brush.cast(), path);
                gp::GdipDeleteBrush(brush.cast());
            }
        }
        gp::GdipDeletePath(path);
    }
    unsafe fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: u32) {
        let mut pen = ptr::null_mut();
        if gp::GdipCreatePen1(color, width, gp::UnitPixel, &mut pen) == 0 {
            gp::GdipSetPenStartCap(pen, gp::LineCapRound);
            gp::GdipSetPenEndCap(pen, gp::LineCapRound);
            gp::GdipDrawLine(self.0, pen, x1, y1, x2, y2);
            gp::GdipDeletePen(pen);
        }
    }
    unsafe fn arc(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        start: f32,
        sweep: f32,
        width: f32,
        color: u32,
    ) {
        let mut pen = ptr::null_mut();
        if gp::GdipCreatePen1(color, width, gp::UnitPixel, &mut pen) == 0 {
            gp::GdipSetPenStartCap(pen, gp::LineCapRound);
            gp::GdipSetPenEndCap(pen, gp::LineCapRound);
            gp::GdipDrawArc(self.0, pen, x, y, w, h, start, sweep);
            gp::GdipDeletePen(pen);
        }
    }
    unsafe fn mic(&self, x: f32, y: f32, size: f32, color: u32) {
        self.rounded(
            x + size * 0.34,
            y,
            size * 0.32,
            size * 0.58,
            size * 0.16,
            color,
            false,
        );
        self.arc(
            x + size * 0.18,
            y + size * 0.23,
            size * 0.64,
            size * 0.54,
            0.,
            180.,
            size * 0.065,
            color,
        );
        self.line(
            x + size * 0.5,
            y + size * 0.77,
            x + size * 0.5,
            y + size * 0.96,
            size * 0.065,
            color,
        );
        self.line(
            x + size * 0.32,
            y + size * 0.97,
            x + size * 0.68,
            y + size * 0.97,
            size * 0.065,
            color,
        );
    }
}
impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            gp::GdipDeleteGraphics(self.0);
        }
    }
}

pub unsafe fn background(dc: HDC, dpi: u32, enabled: bool, peak: u32, frame: u64) {
    let Some(c) = Canvas::new(dc, dpi) else {
        return;
    };
    let accent = if enabled { GREEN } else { 0xff8190a2 };
    c.ellipse(
        150.,
        58.,
        100.,
        100.,
        if enabled { 0xffedf8f3 } else { 0xfff0f3f7 },
    );
    c.ellipse(162., 70., 76., 76., accent);
    c.rounded(193., 86., 14., 28., 7., 0xffffffff, false);
    c.arc(186., 96., 28., 28., 0., 180., 2.5, 0xffffffff);
    c.line(200., 124., 200., 134., 2.5, 0xffffffff);
    c.line(192., 134., 208., 134., 2.5, 0xffffffff);
    for i in 0..28 {
        let activity = (peak as f32 / 8000.).clamp(0., 1.);
        let shape = ((i as f32 * 1.13 - frame as f32 * 0.32).sin().abs() * 0.75 + 0.25)
            * (1. - (i as f32 - 14.).abs() / 22.);
        let h = if peak > 0 {
            5. + 16. * activity * shape
        } else {
            4. + 2. * ((i as f32 * 1.13).sin().abs())
        };
        c.rounded(
            90.5 + i as f32 * 8.,
            174. - h / 2.,
            3.,
            h,
            1.5,
            if peak > 0 && enabled {
                accent
            } else {
                0xffcbd4df
            },
            false,
        );
    }
    for y in [230., 288., 346., 404., 462.] {
        c.line(20., y, 380., y, 1., LINE);
    }
    // Service, source, microphone, silence and keyboard row glyphs.
    let gear = (0..48)
        .map(|i| {
            let a = i as f32 * std::f32::consts::TAU / 48.;
            let r = if matches!(i % 6, 1..=3) { 9. } else { 7. };
            (30. + a.cos() * r, 259. + a.sin() * r)
        })
        .collect::<Vec<_>>();
    c.polygon(&gear, MUTED, true);
    c.arc(27., 256., 6., 6., 0., 360., 1.5, MUTED);
    for (i, h) in [8., 14., 20., 12., 6.].iter().enumerate() {
        c.line(
            22. + i as f32 * 4.,
            317. - h / 2.,
            22. + i as f32 * 4.,
            317. + h / 2.,
            1.5,
            MUTED,
        );
    }
    c.rounded(27., 365., 6., 11., 3., MUTED, true);
    c.arc(24., 369., 12., 12., 0., 180., 1.5, MUTED);
    c.line(30., 381., 30., 385., 1.5, MUTED);
    c.line(27., 385., 33., 385., 1.5, MUTED);
    c.arc(21., 424., 18., 18., 0., 360., 1.5, MUTED);
    c.line(30., 427., 30., 433., 1.5, MUTED);
    c.line(30., 433., 34., 435., 1.5, MUTED);
    c.rounded(20., 484., 20., 14., 2.5, MUTED, true);
    for row in 0..2 {
        for col in 0..5 {
            c.ellipse(
                23. + col as f32 * 3.5,
                487. + row as f32 * 3.5,
                1.3,
                1.3,
                MUTED,
            );
        }
    }
    c.line(25., 495., 35., 495., 1.3, MUTED);
    c.rounded(284., 476., 36., 30., 6., LINE, true);
    for points in [
        vec![(293., 485.), (300., 484.), (300., 490.), (293., 490.)],
        vec![(302., 483.7), (311., 482.), (311., 490.), (302., 490.)],
        vec![(293., 492.), (300., 492.), (300., 498.), (293., 496.8)],
        vec![(302., 492.), (311., 492.), (311., 500.), (302., 498.5)],
    ] {
        c.polygon(&points, 0xff273548, false);
    }
    c.rounded(348., 476., 32., 30., 6., LINE, true);
}

pub unsafe fn value_button(dc: HDC, dpi: u32, width: i32, height: i32, pressed: bool) {
    let Some(c) = Canvas::new(dc, dpi) else {
        return;
    };
    let w = width as f32 * 96. / dpi as f32;
    let h = height as f32 * 96. / dpi as f32;
    c.rounded(
        0.5,
        0.5,
        w - 1.,
        h - 1.,
        6.,
        if pressed { 0xffedf3f8 } else { 0xfffcfdff },
        false,
    );
    c.rounded(0.5, 0.5, w - 1., h - 1., 6., 0xffdbe3ec, true);
    c.line(w - 23., h / 2. - 2., w - 19., h / 2. + 2., 1.5, MUTED);
    c.line(w - 19., h / 2. + 2., w - 15., h / 2. - 2., 1.5, MUTED);
}

pub unsafe fn menu_row(dc: HDC, dpi: u32, rect: &RECT, hover: bool, checked: bool) {
    let Some(c) = Canvas::new(dc, dpi) else {
        return;
    };
    let ratio = dpi as f32 / 96.;
    let x = rect.left as f32 / ratio;
    let y = rect.top as f32 / ratio;
    let w = (rect.right - rect.left) as f32 / ratio;
    let h = (rect.bottom - rect.top) as f32 / ratio;
    if hover || checked {
        c.rounded(
            x + 3.,
            y + 2.,
            w - 6.,
            h - 4.,
            5.,
            if hover { 0xffedf3f6 } else { 0xffeef8f2 },
            false,
        );
    }
    if checked {
        c.line(x + 13., y + h / 2., x + 16., y + h / 2. + 3., 1.5, GREEN);
        c.line(
            x + 16.,
            y + h / 2. + 3.,
            x + 22.,
            y + h / 2. - 4.,
            1.5,
            GREEN,
        );
    }
}

pub unsafe fn update_bubble(dc: HDC, dpi: u32, available: bool) {
    let Some(c) = Canvas::new(dc, dpi) else {
        return;
    };
    let h = if available { 126. } else { 78. };
    c.rounded(14., 95., 268., h, 10., 0x180f172a, false);
    c.rounded(12., 92., 268., h, 10., 0xfffafcff, false);
    c.rounded(12., 92., 268., h, 10., 0xffdbe3ec, true);
    c.polygon(&[(24., 93.), (30., 86.), (36., 93.)], 0xfffafcff, false);
}

pub unsafe fn update_badge(dc: HDC, dpi: u32) {
    let Some(c) = Canvas::new(dc, dpi) else {
        return;
    };
    c.ellipse(23., 3., 8., 8., 0xfff2f7fc);
    c.ellipse(24., 4., 6., 6., GREEN);
}

pub unsafe fn close_glyph(dc: HDC, dpi: u32, width: i32, height: i32) {
    let Some(c) = Canvas::new(dc, dpi) else {
        return;
    };
    let x = width as f32 * 48. / dpi as f32;
    let y = height as f32 * 48. / dpi as f32;
    c.line(x - 3., y - 3., x + 3., y + 3., 1.3, MUTED);
    c.line(x - 3., y + 3., x + 3., y - 3., 1.3, MUTED);
}

pub unsafe fn button(
    dc: HDC,
    dpi: u32,
    width: i32,
    height: i32,
    toggle: bool,
    selected: bool,
    disabled: bool,
    pressed: bool,
) {
    let Some(c) = Canvas::new(dc, dpi) else {
        return;
    };
    let w = width as f32 * 96. / dpi as f32;
    let h = height as f32 * 96. / dpi as f32;
    if toggle {
        let fill = if disabled {
            0xffd5dce5
        } else if selected {
            GREEN
        } else {
            0xffbbc5d2
        };
        c.rounded(1., (h - 26.) / 2., w - 2., 26., 13., fill, false);
        c.ellipse(
            if selected { w - 24. } else { 4. },
            (h - 20.) / 2.,
            20.,
            20.,
            0xffffffff,
        );
    } else {
        let fill = if pressed {
            0xffd7eee2
        } else if selected {
            0xffe7f6ee
        } else {
            0xfff0f3f7
        };
        c.rounded(0.5, 0.5, w - 1., h - 1., 6., fill, false);
        c.rounded(
            0.5,
            0.5,
            w - 1.,
            h - 1.,
            6.,
            if selected { 0xffa7d9be } else { LINE },
            true,
        );
    }
}

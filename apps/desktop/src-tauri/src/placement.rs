// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Physical-coordinate popup placement shared by Mac status bar and Windows notification area.
#[derive(Clone, Copy, Debug)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
pub fn popup(anchor: Area, work: Area, scale: f64) -> Placement {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let margin = 6.0 * scale;
    let width = (380.0 * scale).min((work.width - margin * 2.0).max(1.0));
    let height = (580.0 * scale).min((work.height - margin * 2.0).max(1.0));
    let x = anchor.x + anchor.width / 2.0 - width / 2.0;
    // Choose the side with most room: below a top menu bar, above a bottom taskbar,
    // and adapt to top/side taskbars without relying on the OS name.
    let below = work.y + work.height - (anchor.y + anchor.height);
    let above = anchor.y - work.y;
    let y = if below >= height + margin || below >= above {
        anchor.y + anchor.height + margin
    } else {
        anchor.y - height - margin
    };
    let clamp = |value: f64, start: f64, extent: f64, length: f64| {
        value
            .max(start + margin)
            .min((start + extent - length - margin).max(start + margin))
    };
    Placement {
        x: clamp(x, work.x, work.width, width).round() as i32,
        y: clamp(y, work.y, work.height, height).round() as i32,
        width: width.round() as u32,
        height: height.round() as u32,
    }
}

// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! AppKit text rasterization into a transparent 3x template image (88 x 18 pt).
use objc2::{runtime::AnyObject, AnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSBezierPath, NSBitmapFormat, NSBitmapImageRep, NSColor, NSDeviceRGBColorSpace, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSGraphicsContext, NSStringDrawing,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};

const SCALE: f64 = 3.0;
const WIDTH: usize = 264;
const HEIGHT: usize = 54;

struct GraphicsGuard;
impl Drop for GraphicsGuard {
    fn drop(&mut self) {
        NSGraphicsContext::restoreGraphicsState_class();
    }
}
/// Fixed-width startup fallback when AppKit cannot allocate a drawing context.
/// No title-based/side-by-side speed fallback; the tooltip carries the error.
pub fn fallback() -> tauri::image::Image<'static> {
    let mut rgba = vec![0u8; WIDTH * HEIGHT * 4];
    for (x, top) in [(6, 31), (18, 19), (30, 7)] {
        for y in top..47 {
            for x in x..x + 6 {
                rgba[(y * WIDTH + x) * 4 + 3] = 255;
            }
        }
    }
    tauri::image::Image::new_owned(rgba, WIDTH as u32, HEIGHT as u32)
}

pub fn render(lines: &[String; 2]) -> Result<tauri::image::Image<'static>, String> {
    let _main = MainThreadMarker::new().ok_or("菜单栏绘制必须在主线程运行")?;
    objc2::rc::autoreleasepool(|_| {
        // Render with vertical padding first: NSString's line metrics do not include
        // every glyph's ink (notably '/'). Center actual alpha bounds into each 9pt
        // row afterwards, so units, arrows and CJK status text are never clipped.
        const SCRATCH_HEIGHT: usize = 128;
        let bitmap = unsafe {
            // SAFETY: null planes asks AppKit to allocate owned interleaved RGBA.
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(), std::ptr::null_mut(), WIDTH as isize, SCRATCH_HEIGHT as isize,
                8, 4, true, false, NSDeviceRGBColorSpace, NSBitmapFormat::empty(),
                (WIDTH * 4) as isize, 32)
        }.ok_or("无法创建菜单栏位图")?;
        let pointer = bitmap.bitmapData();
        if pointer.is_null() || bitmap.bytesPerRow() != (WIDTH * 4) as isize {
            return Err("菜单栏位图格式不可用".into());
        }
        // SAFETY: checked allocation/stride; the bitmap is exclusive to this GUI call.
        unsafe {
            std::ptr::write_bytes(pointer, 0, WIDTH * SCRATCH_HEIGHT * 4);
        }
        let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap)
            .ok_or("无法创建菜单栏绘制上下文")?;
        NSGraphicsContext::saveGraphicsState_class();
        let _guard = GraphicsGuard;
        NSGraphicsContext::setCurrentContext(Some(&context));
        context.setShouldAntialias(true);
        NSColor::blackColor().set();
        for (x, height) in [(2.0, 5.0), (6.0, 9.0), (10.0, 13.0)] {
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(x * SCALE, (SCRATCH_HEIGHT - HEIGHT) as f64 + 2.5 * SCALE),
                NSSize::new(2.0 * SCALE, height * SCALE),
            ));
        }
        // Approximately 9pt; 8.5pt leaves room for the full slash/arrow glyph ink.
        let font = NSFont::monospacedDigitSystemFontOfSize_weight(8.5 * SCALE, 0.0);
        let color = NSColor::blackColor();
        let attrs: objc2::rc::Retained<NSDictionary<NSString, AnyObject>> = unsafe {
            // SAFETY: these keys require NSFont and NSColor respectively.
            NSDictionary::from_slices(
                &[NSFontAttributeName, NSForegroundColorAttributeName],
                &[font.as_ref() as &AnyObject, color.as_ref() as &AnyObject],
            )
        };
        for (index, line) in lines.iter().enumerate() {
            let y = if index == 0 { 86.0 } else { 22.0 };
            let draw =
                |text: &str, edge: f64, width: f64, align_right: bool| -> Result<(), String> {
                    let text = NSString::from_str(text);
                    let size = unsafe { text.sizeWithAttributes(Some(&attrs)) };
                    if size.width > width * SCALE {
                        return Err("菜单栏读数超过固定列宽".into());
                    }
                    let x = if align_right {
                        edge * SCALE - size.width
                    } else {
                        edge * SCALE
                    };
                    unsafe {
                        text.drawAtPoint_withAttributes(NSPoint::new(x, y), Some(&attrs));
                    }
                    Ok(())
                };
            // Fixed arrow, number and unit columns; status item width never changes.
            draw(if index == 0 { "↑" } else { "↓" }, 15.0, 11.0, false)?;
            if let Some((number, unit)) = line.rsplit_once(' ') {
                draw(number, 61.0, 35.0, true)?;
                draw(unit, 64.0, 23.0, false)?;
            } else {
                draw(line, 87.0, 61.0, true)?;
            }
        }
        context.flushGraphics();
        // SAFETY: fixed-size live AppKit allocation; black premultiplied RGB is also
        // valid straight RGBA, and template rendering consumes only the alpha mask.
        let source = unsafe { std::slice::from_raw_parts(pointer, WIDTH * SCRATCH_HEIGHT * 4) };
        let mut rgba = vec![0u8; WIDTH * HEIGHT * 4];
        const TEXT_X: usize = 42;
        for y in 0..HEIGHT {
            rgba[y * WIDTH * 4..(y * WIDTH + TEXT_X) * 4]
                .copy_from_slice(&source[y * WIDTH * 4..(y * WIDTH + TEXT_X) * 4]);
        }
        for index in 0..2 {
            let has_ink = |y: &usize| {
                source[(*y * WIDTH + TEXT_X) * 4..(*y + 1) * WIDTH * 4]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| pixel[3] != 0)
            };
            let area = index * 64..(index + 1) * 64;
            let first = area.clone().find(has_ink).ok_or("菜单栏文字绘制为空")?;
            let last = area.rev().find(has_ink).ok_or("菜单栏文字绘制为空")?;
            let height = last - first + 1;
            if height > HEIGHT / 2 {
                return Err("菜单栏文字超过固定行高".into());
            }
            let top = index * (HEIGHT / 2) + (HEIGHT / 2 - height) / 2;
            for dy in 0..height {
                let from = ((first + dy) * WIDTH + TEXT_X) * 4;
                let to = ((top + dy) * WIDTH + TEXT_X) * 4;
                let bytes = (WIDTH - TEXT_X) * 4;
                rgba[to..to + bytes].copy_from_slice(&source[from..from + bytes]);
            }
        }
        Ok(tauri::image::Image::new_owned(
            rgba,
            WIDTH as u32,
            HEIGHT as u32,
        ))
    })
}

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};

use cap_media::platform::Bounds;
use cap_project::{CursorClickEvent, CursorMoveEvent, XY};
use cap_utils::spawn_actor;
use device_query::{DeviceQuery, DeviceState};
use image::GenericImageView;
use tokio::sync::oneshot;
use tracing::{debug, error, info};

pub struct Cursor {
    pub file_name: String,
    pub id: u32,
    pub hotspot: XY<f64>,
}

pub type Cursors = HashMap<u64, Cursor>;

pub struct CursorActorResponse {
    // pub cursor_images: HashMap<String, Vec<u8>>,
    pub cursors: Cursors,
    pub next_cursor_id: u32,
    pub moves: Vec<CursorMoveEvent>,
    pub clicks: Vec<CursorClickEvent>,
}

pub struct CursorActor {
    stop_signal: Arc<AtomicBool>,
    rx: oneshot::Receiver<CursorActorResponse>,
}

impl CursorActor {
    pub async fn stop(self) -> CursorActorResponse {
        self.stop_signal
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.rx.await.unwrap()
    }
}

#[tracing::instrument(name = "cursor", skip_all)]
pub fn spawn_cursor_recorder(
    screen_bounds: Bounds,
    cursors_dir: PathBuf,
    prev_cursors: Cursors,
    next_cursor_id: u32,
) -> CursorActor {
    let stop_signal = Arc::new(AtomicBool::new(false));
    let (tx, rx) = oneshot::channel();

    spawn_actor({
        let stop_signal = stop_signal.clone();
        async move {
            let device_state = DeviceState::new();
            let mut last_mouse_state = device_state.get_mouse();
            let start_time = Instant::now();

            let mut response = CursorActorResponse {
                cursors: prev_cursors,
                next_cursor_id,
                moves: vec![],
                clicks: vec![],
            };

            // Create cursors directory if it doesn't exist
            std::fs::create_dir_all(&cursors_dir).unwrap();

            while !stop_signal.load(std::sync::atomic::Ordering::Relaxed) {
                let mouse_state = device_state.get_mouse();
                let elapsed = start_time.elapsed().as_secs_f64() * 1000.0;
                let unix_time = chrono::Utc::now().timestamp_millis() as f64;

                let cursor_data = get_cursor_image_data();
                let cursor_id = if let Some(data) = cursor_data {
                    let mut hasher = DefaultHasher::default();
                    data.image.hash(&mut hasher);
                    let id = hasher.finish();

                    // Check if we've seen this cursor data before
                    if let Some(existing_id) = response.cursors.get(&id) {
                        existing_id.id.to_string()
                    } else {
                        // New cursor data - save it
                        let cursor_id = response.next_cursor_id.to_string();
                        let file_name = format!("cursor_{}.png", cursor_id);
                        let cursor_path = cursors_dir.join(&file_name);

                        if let Ok(image) = image::load_from_memory(&data.image) {
                            dbg!(image.dimensions());
                            // Convert to RGBA
                            let rgba_image = image.into_rgba8();

                            if let Err(e) = rgba_image.save(&cursor_path) {
                                error!("Failed to save cursor image: {}", e);
                            } else {
                                info!("Saved cursor {cursor_id} image to: {:?}", file_name);
                                response.cursors.insert(
                                    id,
                                    Cursor {
                                        file_name,
                                        id: response.next_cursor_id,
                                        hotspot: data.hotspot,
                                    },
                                );
                                response.next_cursor_id += 1;
                            }
                        }

                        cursor_id
                    }
                } else {
                    "default".to_string()
                };

                if mouse_state.coords != last_mouse_state.coords {
                    let mouse_event = CursorMoveEvent {
                        active_modifiers: vec![],
                        cursor_id: cursor_id.clone(),
                        process_time_ms: elapsed,
                        unix_time_ms: unix_time,
                        x: (mouse_state.coords.0 as f64 - screen_bounds.x) / screen_bounds.width,
                        y: (mouse_state.coords.1 as f64 - screen_bounds.y) / screen_bounds.height,
                    };
                    response.moves.push(mouse_event);
                }

                for (num, &pressed) in mouse_state.button_pressed.iter().enumerate() {
                    let Some(prev) = last_mouse_state.button_pressed.get(num) else {
                        continue;
                    };

                    if pressed == *prev {
                        continue;
                    }

                    let mouse_event = CursorClickEvent {
                        down: pressed,
                        active_modifiers: vec![],
                        cursor_num: num as u8,
                        cursor_id: cursor_id.clone(),
                        process_time_ms: elapsed,
                        unix_time_ms: unix_time,
                        x: (mouse_state.coords.0 as f64 - screen_bounds.x) / screen_bounds.width,
                        y: (mouse_state.coords.1 as f64 - screen_bounds.y) / screen_bounds.height,
                    };
                    response.clicks.push(mouse_event);
                }

                last_mouse_state = mouse_state;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            tx.send(response).ok();
        }
    });

    CursorActor { rx, stop_signal }
}

#[derive(Debug)]
struct CursorData {
    image: Vec<u8>,
    hotspot: XY<f64>,
}

#[cfg(target_os = "macos")]
fn get_cursor_image_data() -> Option<CursorData> {
    use cocoa::base::{id, nil};
    use cocoa::foundation::{NSPoint, NSSize, NSUInteger};
    use objc::rc::autoreleasepool;
    use objc::runtime::Class;
    use objc::*;

    autoreleasepool(|| {
        let nscursor_class = match Class::get("NSCursor") {
            Some(cls) => cls,
            None => return None,
        };

        unsafe {
            // Get the current system cursor
            let current_cursor: id = msg_send![nscursor_class, currentSystemCursor];
            if current_cursor == nil {
                return None;
            }

            // Get the image of the cursor
            let cursor_image: id = msg_send![current_cursor, image];
            if cursor_image == nil {
                return None;
            }

            let cursor_size: NSSize = msg_send![cursor_image, size];
            let cursor_hotspot: NSPoint = msg_send![current_cursor, hotSpot];

            // Get the TIFF representation of the image
            let image_data: id = msg_send![cursor_image, TIFFRepresentation];
            if image_data == nil {
                return None;
            }

            // Get the length of the data
            let length: NSUInteger = msg_send![image_data, length];

            // Get the bytes of the data
            let bytes: *const u8 = msg_send![image_data, bytes];

            // Copy the data into a Vec<u8>
            let slice = std::slice::from_raw_parts(bytes, length as usize);
            let data = slice.to_vec();

            Some(CursorData {
                image: data,
                hotspot: XY::new(
                    cursor_hotspot.x / cursor_size.width,
                    cursor_hotspot.y / cursor_size.height,
                ),
            })
        }
    })
}

#[cfg(windows)]
fn get_cursor_image_data() -> Option<CursorData> {
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetObjectA,
        ReleaseDC, SelectObject, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSORINFO_FLAGS};
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    unsafe {
        // Get cursor info
        let mut cursor_info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            flags: CURSORINFO_FLAGS(0),
            hCursor: Default::default(),
            ptScreenPos: POINT::default(),
        };

        // Handle Result return type
        if GetCursorInfo(&mut cursor_info).is_err() {
            return None;
        }

        // If no cursor, return None
        if cursor_info.hCursor.is_invalid() {
            return None;
        }

        // Get icon info
        let mut icon_info = ICONINFO::default();
        // Handle Result return type
        if GetIconInfo(cursor_info.hCursor, &mut icon_info).is_err() {
            return None;
        }

        // Get bitmap info
        let mut bitmap = BITMAP::default();
        if GetObjectA(
            icon_info.hbmColor,
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bitmap as *mut _ as *mut _),
        ) == 0
        {
            return None;
        }

        // Create compatible DC
        let screen_dc = GetDC(HWND::default());
        let mem_dc = CreateCompatibleDC(screen_dc);

        // Create bitmap info header
        let bi = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bitmap.bmWidth,
            biHeight: -bitmap.bmHeight, // Negative height for top-down bitmap
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };

        let bitmap_info = BITMAPINFO {
            bmiHeader: bi,
            bmiColors: [Default::default()],
        };

        // Create DIB section
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(mem_dc, &bitmap_info, DIB_RGB_COLORS, &mut bits, None, 0);

        if dib.is_err() {
            return None;
        }

        let dib = dib.unwrap();

        // Select DIB into DC
        let old_bitmap = SelectObject(mem_dc, dib);

        // Copy cursor image
        if BitBlt(
            mem_dc,
            0,
            0,
            bitmap.bmWidth,
            bitmap.bmHeight,
            screen_dc,
            cursor_info.ptScreenPos.x,
            cursor_info.ptScreenPos.y,
            SRCCOPY,
        )
        .is_err()
        {
            return None;
        }

        // Get image data
        let size = (bitmap.bmWidth * bitmap.bmHeight * 4) as usize;
        let mut image_data = vec![0u8; size];
        std::ptr::copy_nonoverlapping(bits, image_data.as_mut_ptr() as *mut _, size);

        // Cleanup
        SelectObject(mem_dc, old_bitmap);
        DeleteObject(dib);
        DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);
        DeleteObject(icon_info.hbmColor);
        DeleteObject(icon_info.hbmMask);

        // Convert to PNG format
        let image =
            image::RgbaImage::from_raw(bitmap.bmWidth as u32, bitmap.bmHeight as u32, image_data)?;

        let mut png_data = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut png_data),
                image::ImageFormat::Png,
            )
            .ok()?;

        Some(CursorData {
            image: png_data,
            hotspot: XY::new(0.0, 0.0),
        })
    }
}

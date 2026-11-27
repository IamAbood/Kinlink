//! QR scanner for adding friends: the back camera (NDK Camera2) feeds grey frames to rqrr.
//! Runs on its own thread; the UI reads the latest frame (for the preview) and the found text.
use ndk::media::image_reader::{AcquireResult, ImageFormat, ImageReader};
use ndk_sys as c;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[link(name = "camera2ndk")]
unsafe extern "C" {}

const W: i32 = 640;
const H: i32 = 480;

/// Grey preview (width, height, pixels), already turned upright for a portrait phone.
pub type Frame = (usize, usize, Vec<u8>);

pub struct Scanner {
    pub frame: Arc<Mutex<Option<Frame>>>,
    pub found: Arc<Mutex<Option<Result<String, String>>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Scanner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn start(wake: impl Fn() + Send + 'static) -> Scanner {
    let s = Scanner { frame: Default::default(), found: Default::default(), stop: Default::default() };
    let (frame, found, stop) = (s.frame.clone(), s.found.clone(), s.stop.clone());
    std::thread::spawn(move || {
        let r = unsafe { run(&frame, &stop, &wake) };
        *found.lock().unwrap() = Some(r);
        wake();
    });
    s
}

unsafe extern "C" fn dev_cb(_: *mut c_void, _: *mut c::ACameraDevice) {}
unsafe extern "C" fn dev_err(_: *mut c_void, _: *mut c::ACameraDevice, e: i32) {
    log::error!("camera error {e}");
}
unsafe extern "C" fn sess_cb(_: *mut c_void, _: *mut c::ACameraCaptureSession) {}

fn ok(st: c::camera_status_t, what: &str) -> Result<(), String> {
    if st.0 == 0 { Ok(()) } else { Err(format!("{what} failed ({})", st.0)) }
}

/// Opens the back camera, scans until a QR code reads or `stop` is set. Ok = the QR text.
unsafe fn run(frame: &Mutex<Option<Frame>>, stop: &AtomicBool, wake: &dyn Fn()) -> Result<String, String> {
    unsafe {
        let reader = ImageReader::new(W, H, ImageFormat::YUV_420_888, 2).map_err(|e| e.to_string())?;
        let window = reader.window().map_err(|e| e.to_string())?;
        let win = window.ptr().as_ptr();

        let mgr = c::ACameraManager_create();
        let mut ids = null_mut();
        let mut dev = null_mut();
        let mut out = null_mut();
        let mut cont = null_mut();
        let mut target = null_mut();
        let mut req = null_mut();
        let mut sess = null_mut();
        let mut dcb = c::ACameraDevice_StateCallbacks { context: null_mut(), onDisconnected: Some(dev_cb), onError: Some(dev_err) };
        let scb = c::ACameraCaptureSession_stateCallbacks { context: null_mut(), onClosed: Some(sess_cb), onReady: Some(sess_cb), onActive: Some(sess_cb) };
        let r = (|| {
            ok(c::ACameraManager_getCameraIdList(mgr, &mut ids), "camera list")?;
            let list = std::slice::from_raw_parts((*ids).cameraIds, (*ids).numCameras.max(0) as usize);
            // The back camera, else whichever comes first.
            let back = list.iter().copied().find(|&id| {
                let mut meta = null_mut();
                if c::ACameraManager_getCameraCharacteristics(mgr, id, &mut meta).0 != 0 {
                    return false;
                }
                let mut e: c::ACameraMetadata_const_entry = std::mem::zeroed();
                let b = c::ACameraMetadata_getConstEntry(meta, c::acamera_metadata_tag::ACAMERA_LENS_FACING.0, &mut e).0 == 0
                    && e.count > 0
                    && *e.data.u8_ as u32 == c::acamera_metadata_enum_acamera_lens_facing::ACAMERA_LENS_FACING_BACK.0;
                c::ACameraMetadata_free(meta);
                b
            });
            let id = back.or(list.first().copied()).ok_or("This phone has no camera")?;
            ok(c::ACameraManager_openCamera(mgr, id, &mut dcb, &mut dev), "open camera")?;
            ok(c::ACaptureSessionOutput_create(win, &mut out), "camera output")?;
            ok(c::ACaptureSessionOutputContainer_create(&mut cont), "camera outputs")?;
            ok(c::ACaptureSessionOutputContainer_add(cont, out), "camera outputs")?;
            ok(c::ACameraOutputTarget_create(win, &mut target), "camera target")?;
            ok(c::ACameraDevice_createCaptureRequest(dev, c::ACameraDevice_request_template::TEMPLATE_PREVIEW, &mut req), "camera request")?;
            ok(c::ACaptureRequest_addTarget(req, target), "camera target")?;
            let af = c::acamera_metadata_enum_acamera_control_af_mode::ACAMERA_CONTROL_AF_MODE_CONTINUOUS_PICTURE.0 as u8;
            c::ACaptureRequest_setEntry_u8(req, c::acamera_metadata_tag::ACAMERA_CONTROL_AF_MODE.0, 1, &af);
            ok(c::ACameraDevice_createCaptureSession(dev, cont, &scb, &mut sess), "camera session")?;
            ok(c::ACameraCaptureSession_setRepeatingRequest(sess, null_mut(), 1, &mut req, null_mut()), "camera start")?;
            while !stop.load(Ordering::Relaxed) {
                let img = match reader.acquire_latest_image() {
                    Ok(AcquireResult::Image(i)) => i,
                    _ => {
                        std::thread::sleep(std::time::Duration::from_millis(30));
                        continue;
                    }
                };
                let (Ok(y), Ok(rs), Ok(w), Ok(h)) = (img.plane_data(0), img.plane_row_stride(0), img.width(), img.height()) else { continue };
                let (w, h, rs) = (w as usize, h as usize, rs as usize);
                // Turn 90° clockwise: the sensor is landscape, the phone is held upright.
                let mut up = vec![0u8; w * h];
                for yy in 0..h {
                    let row = &y[yy * rs..yy * rs + w];
                    for (xx, &p) in row.iter().enumerate() {
                        up[xx * h + (h - 1 - yy)] = p;
                    }
                }
                drop(img);
                if let Some(text) = decode(h, w, &up) {
                    return Ok(text);
                }
                *frame.lock().unwrap() = Some((h, w, up));
                wake();
            }
            Err(String::new()) // cancelled
        })();
        if !sess.is_null() {
            c::ACameraCaptureSession_stopRepeating(sess);
            c::ACameraCaptureSession_close(sess);
        }
        if !req.is_null() {
            c::ACaptureRequest_free(req);
        }
        if !target.is_null() {
            c::ACameraOutputTarget_free(target);
        }
        if !cont.is_null() {
            c::ACaptureSessionOutputContainer_free(cont);
        }
        if !out.is_null() {
            c::ACaptureSessionOutput_free(out);
        }
        if !dev.is_null() {
            c::ACameraDevice_close(dev);
        }
        if !ids.is_null() {
            c::ACameraManager_deleteCameraIdList(ids);
        }
        c::ACameraManager_delete(mgr);
        drop(window);
        drop(reader);
        r
    }
}

/// The first QR code's text in a grey image, if any.
pub fn decode(w: usize, h: usize, px: &[u8]) -> Option<String> {
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| px[y * w + x]);
    img.detect_grids().into_iter().find_map(|g| g.decode().ok().map(|(_, s)| s))
}

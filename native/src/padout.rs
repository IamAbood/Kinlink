//! Rumble for PlayStation controllers on the guest's PC (XInput only covers Xbox-style pads).
//! Writes the controller's own HID output report: DualShock 4 / DualSense, USB or Bluetooth.
use hidapi::{BusType, HidApi};
use std::sync::Mutex;

const SONY: u16 = 0x054C;
const DS4: &[u16] = &[0x05C4, 0x09CC, 0x0BA0];
const DUALSENSE: &[u16] = &[0x0CE6, 0x0DF2];

static API: Mutex<Option<HidApi>> = Mutex::new(None);
/// Open controllers, kept so frequent rumble updates don't reopen the device each time.
static OPEN: Mutex<Vec<((u16, usize), hidapi::HidDevice, bool)>> = Mutex::new(Vec::new());

/// CRC32 (IEEE) as the controllers expect it for Bluetooth output reports.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Builds the output report that sets the motors (large = left/strong, small = right/weak).
pub fn report(product: u16, bluetooth: bool, large: u8, small: u8) -> Option<Vec<u8>> {
    let with_crc = |mut r: Vec<u8>| {
        let n = r.len() - 4;
        let crc = crc32(&[&[0xA2u8][..], &r[..n]].concat());
        r[n..].copy_from_slice(&crc.to_le_bytes());
        r
    };
    if DS4.contains(&product) {
        Some(if bluetooth {
            let mut r = vec![0u8; 78];
            r[0] = 0x11;
            r[1] = 0xC0; // HID + CRC
            r[3] = 0x07; // enable rumble + lightbar + flash
            r[6] = small;
            r[7] = large;
            with_crc(r)
        } else {
            let mut r = vec![0u8; 32];
            r[0] = 0x05;
            r[1] = 0x07;
            r[4] = small;
            r[5] = large;
            r
        })
    } else if DUALSENSE.contains(&product) {
        Some(if bluetooth {
            let mut r = vec![0u8; 78];
            r[0] = 0x31;
            r[1] = 0x02;
            r[2] = 0x03; // compatible vibration + haptics select
            r[4] = small;
            r[5] = large;
            with_crc(r)
        } else {
            let mut r = vec![0u8; 48];
            r[0] = 0x02;
            r[1] = 0x03;
            r[3] = small;
            r[4] = large;
            r
        })
    } else {
        None
    }
}

/// Vibrates the `nth` connected Sony controller with this product id.
pub fn rumble(product: u16, nth: usize, large: u8, small: u8) -> Result<(), String> {
    {
        let mut open = OPEN.lock().unwrap();
        if let Some(i) = open.iter().position(|(k, _, _)| *k == (product, nth)) {
            let r = report(product, open[i].2, large, small).ok_or("not a PlayStation controller")?;
            if open[i].1.write(&r).is_ok() {
                return Ok(());
            }
            open.remove(i); // unplugged or re-paired: open it again below
        }
    }
    let mut guard = API.lock().unwrap();
    if guard.is_none() {
        *guard = Some(HidApi::new().map_err(|e| e.to_string())?);
    }
    let api = guard.as_mut().unwrap();
    let _ = api.refresh_devices();
    let dev = api
        .device_list()
        .filter(|d| d.vendor_id() == SONY && d.product_id() == product)
        .nth(nth)
        .ok_or("controller not found")?;
    let bt = matches!(dev.bus_type(), BusType::Bluetooth);
    let r = report(product, bt, large, small).ok_or("not a PlayStation controller")?;
    let h = dev.open_device(api).map_err(|e| e.to_string())?;
    h.write(&r).map_err(|e| e.to_string())?;
    OPEN.lock().unwrap().push(((product, nth), h, bt));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports() {
        let r = report(0x05C4, false, 200, 100).unwrap();
        assert_eq!((r[0], r.len(), r[4], r[5]), (0x05, 32, 100, 200));
        let r = report(0x0CE6, true, 10, 20).unwrap();
        assert_eq!((r[0], r.len(), r[4], r[5]), (0x31, 78, 20, 10));
        // CRC covers 0xA2 + report minus its last 4 bytes, stored little-endian.
        let crc = crc32(&[&[0xA2u8][..], &r[..74]].concat());
        assert_eq!(&r[74..], &crc.to_le_bytes());
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926); // standard CRC32 check value
        assert!(report(0x1234, false, 1, 1).is_none());
    }
}

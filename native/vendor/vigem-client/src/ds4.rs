use std::{fmt, mem, ptr};
use std::borrow::Borrow;
use crate::*;

/// DualShock4 HID Input report.
#[cfg(feature = "unstable_ds4")]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[repr(C)]
pub struct DS4Report {
	pub thumb_lx: u8,
	pub thumb_ly: u8,
	pub thumb_rx: u8,
	pub thumb_ry: u8,
	pub buttons: u16,
	pub special: u8,
	pub trigger_l: u8,
	pub trigger_r: u8,
}
#[cfg(feature = "unstable_ds4")]
impl Default for DS4Report {
	#[inline]
	fn default() -> Self {
		DS4Report {
			thumb_lx: 0x80,
			thumb_ly: 0x80,
			thumb_rx: 0x80,
			thumb_ry: 0x80,
			buttons: 0x8,
			special: 0,
			trigger_l: 0,
			trigger_r: 0,
		}
	}
}

// /// DualShock4 v1 complete HID Input report.
// #[derive(Copy, Clone, Debug, Eq, PartialEq)]
// #[repr(C)]
// pub struct DS4ReportEx {
// 	pub thumb_lx: u8,
// 	pub thumb_ly: u8,
// 	pub thumb_rx: u8,
// 	pub thumb_ry: u8,
// 	pub buttons: u16,
// 	pub special: u8,
// 	pub trigger_l: u8,
// 	pub trigger_r: u8,
// 	pub timestamp: u16,
// 	pub battery_lvl: u8,
// 	pub gyro_x: i16,
// 	pub gyro_y: i16,
// 	pub gyro_z: i16,
// 	pub accel_x: i16,
// 	pub accel_y: i16,
// 	pub accel_z: i16,
// 	pub _unknown1: [u8; 5],
// 	pub battery_lvl_special: u8,
// 	pub _unknown2: [u8; 2],
// 	pub touch_packets_n: u8, // 0x00 to 0x03 (USB max)
// 	pub current_touch: DS4Touch,
// 	pub previous_touch: [DS4Touch; 2],
// }

/// A virtual Sony DualShock 4 (wired).
pub struct DualShock4Wired<CL: Borrow<Client>> {
	client: CL,
	event: Event,
	serial_no: u32,
	id: TargetId,
}

impl<CL: Borrow<Client>> DualShock4Wired<CL> {
	/// Creates a new instance.
	#[inline]
	pub fn new(client: CL, id: TargetId) -> DualShock4Wired<CL> {
		let event = Event::new(false, false);
		DualShock4Wired { client, event, serial_no: 0, id }
	}

	/// Returns if the controller is plugged in.
	#[inline]
	pub fn is_attached(&self) -> bool {
		self.serial_no != 0
	}

	/// Returns the id the controller was constructed with.
	#[inline]
	pub fn id(&self) -> TargetId {
		self.id
	}

	/// Returns the client.
	#[inline]
	pub fn client(&self) -> &CL {
		&self.client
	}

	/// Unplugs and destroys the controller, returning the client.
	#[inline]
	pub fn drop(mut self) -> CL {
		let _ = self.unplug();

		unsafe {
			let client = (&self.client as *const CL).read();
			ptr::drop_in_place(&mut self.event);
			mem::forget(self);
			client
		}
	}

	/// Plugs the controller in.
	#[inline(never)]
	pub fn plugin(&mut self) -> Result<(), Error> {
		if self.is_attached() {
			return Err(Error::AlreadyConnected);
		}

		self.serial_no = unsafe {
			let mut plugin = bus::PluginTarget::ds4_wired(1, self.id.vendor, self.id.product);
			let device = self.client.borrow().device;

			// Yes this is how the driver is implemented
			while plugin.ioctl(device, self.event.handle).is_err() {
				plugin.SerialNo += 1;
				if plugin.SerialNo >= u16::MAX as u32 {
					return Err(Error::NoFreeSlot);
				}
			}

			plugin.SerialNo
		};

		Ok(())
	}

	/// Unplugs the controller.
	#[inline(never)]
	pub fn unplug(&mut self) -> Result<(), Error> {
		if !self.is_attached() {
			return Err(Error::NotPluggedIn);
		}

		unsafe {
			let mut unplug = bus::UnplugTarget::new(self.serial_no);
			let device = self.client.borrow().device;
			unplug.ioctl(device, self.event.handle)?;
		}

		self.serial_no = 0;
		Ok(())
	}

	/// Waits until the virtual controller is ready.
	///
	/// Any updates submitted before the virtual controller is ready may return an error.
	#[inline(never)]
	pub fn wait_ready(&mut self) -> Result<(), Error> {
		if !self.is_attached() {
			return Err(Error::NotPluggedIn);
		}

		unsafe {
			let mut wait = bus::WaitDeviceReady::new(self.serial_no);
			let device = self.client.borrow().device;
			wait.ioctl(device, self.event.handle)?;
		}

		Ok(())
	}

	/// Updates the virtual controller state.
	#[cfg(feature = "unstable_ds4")]
	#[inline(never)]
	pub fn update(&mut self, report: &DS4Report) -> Result<(), Error> {
		if !self.is_attached() {
			return Err(Error::NotPluggedIn);
		}

		unsafe {
			let mut dsr = bus::DS4SubmitReport::new(self.serial_no, *report);
			let device = self.client.borrow().device;
			dsr.ioctl(device, self.event.handle)?;
		}

		Ok(())
	}

	// #[inline(never)]
	// pub fn update_ex(&mut self, report: &DS4ReportEx) -> Result<(), Error> {
	// 	if !self.is_attached() {
	// 		return Err(Error::NotPluggedIn);
	// 	}
	// 	unimplemented!()
	// }
}

impl<CL: Borrow<Client>> fmt::Debug for DualShock4Wired<CL> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		f.debug_struct("DualShock4Wired")
			.field("serial_no", &self.serial_no)
			.field("vendor_id", &self.id.vendor)
			.field("product_id", &self.id.product)
			.finish()
	}
}

impl<CL: Borrow<Client>> Drop for DualShock4Wired<CL> {
	#[inline]
	fn drop(&mut self) {
		let _ = self.unplug();
	}
}


// ---------------------------------------------------------------- Parsec Clone patch: DS4 notifications

/// Rumble + lightbar a game sent to a virtual DualShock 4.
#[cfg(feature = "unstable_xtarget_notification")]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct DS4Notification {
	pub large_motor: u8,
	pub small_motor: u8,
	pub lightbar: (u8, u8, u8),
}

#[cfg(feature = "unstable_xtarget_notification")]
pub struct DS4RequestNotification {
	client: Client,
	rn: bus::RequestNotification<bus::DS4RequestNotification>,
	_unpin: std::marker::PhantomPinned,
}

#[cfg(feature = "unstable_xtarget_notification")]
unsafe impl Sync for DS4RequestNotification {}
#[cfg(feature = "unstable_xtarget_notification")]
unsafe impl Send for DS4RequestNotification {}

#[cfg(feature = "unstable_xtarget_notification")]
impl DS4RequestNotification {
	/// Calls `f` for every notification until the target is unplugged.
	pub fn spawn_thread<F: FnMut(DS4Notification) + Send + 'static>(self, mut f: F) -> std::thread::JoinHandle<()> {
		std::thread::spawn(move || {
			let mut reqn = self;
			// Safety: not moved again after this point (lives on this thread's stack until it returns).
			let reqn = unsafe { std::pin::Pin::new_unchecked(&mut reqn) };
			let this = unsafe { reqn.get_unchecked_mut() };
			let device = this.client.device;
			loop {
				if this.rn.buffer.SerialNo == 0 {
					break;
				}
				unsafe { this.rn.ioctl_code(device, bus::IOCTL_DS4_REQUEST_NOTIFICATION) };
				match unsafe { this.rn.poll(device, true) } {
					Ok(()) => f(DS4Notification {
						large_motor: this.rn.buffer.LargeMotor,
						small_motor: this.rn.buffer.SmallMotor,
						lightbar: (this.rn.buffer.Red, this.rn.buffer.Green, this.rn.buffer.Blue),
					}),
					// Aborted when the target is unplugged.
					Err(_) => break,
				}
			}
		})
	}
}

#[cfg(feature = "unstable_xtarget_notification")]
impl Drop for DS4RequestNotification {
	fn drop(&mut self) {
		unsafe {
			let device = self.client.device;
			let _ = self.rn.cancel(device);
		}
	}
}

#[cfg(all(feature = "unstable_ds4", feature = "unstable_xtarget_notification"))]
impl<CL: Borrow<Client>> DualShock4Wired<CL> {
	/// Requests rumble/lightbar notifications for this target.
	pub fn request_notification(&mut self) -> Result<DS4RequestNotification, Error> {
		if !self.is_attached() {
			return Err(Error::NotPluggedIn);
		}
		let client = self.client.borrow().try_clone()?;
		let rn = bus::RequestNotification::new(bus::DS4RequestNotification::new(self.serial_no));
		Ok(DS4RequestNotification { client, rn, _unpin: std::marker::PhantomPinned })
	}
}

//! Devices told the machine is going down: after the filesystems have written
//! back, before power goes or a reset lands.

use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, Mutex};
use slopos_ostd::{KArc, KVec, lock_class};

pub trait DeviceShutdown: Send + Sync {
    fn shutdown(&self);
}

static DEVICES: Mutex<KVec<KArc<dyn DeviceShutdown>>> = Mutex::new(
    KVec::new(),
    lock_class!("DEVICE_SHUTDOWN", LOCK_LEVEL_REGISTRY),
);

pub fn register(device: KArc<dyn DeviceShutdown>) -> bool {
    match DEVICES.lock() {
        Ok(mut devices) => devices.push(device).is_ok(),
        Err(_) => false,
    }
}

/// Every registered device, newest first: a device probed later may sit
/// behind one probed earlier.
pub fn shutdown_devices() {
    let devices = match DEVICES.lock() {
        Ok(mut devices) => core::mem::take(&mut *devices),
        Err(_) => return,
    };
    for device in devices.iter().rev() {
        device.shutdown();
    }
}

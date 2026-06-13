//! CoreAudio property listeners.
//!
//! Registers callbacks against the system audio object so we get notified the
//! moment the device set changes (hotplug) or the system default input device
//! changes (Settings > Sound, AirPods auto-switch, third-party utilities).
//! Removes the need to poll CoreAudio every second from the event loop.
//!
//! The callback fires on a CoreAudio worker thread; it just dispatches a
//! `Message::InputDevicesChanged` via [`EventLoopProxy`], which wakes the main
//! event loop in microseconds.

use crate::event_loop::{EventLoopProxyMessage, Message};
use anyhow::Result;
use log::{trace, warn};
use objc2_core_audio::{
    kAudioDevicePropertyMute, kAudioDevicePropertyScopeInput, kAudioDevicePropertyVolumeScalar,
    kAudioHardwareNoError, kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    AudioObjectAddPropertyListener, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectRemovePropertyListener,
};
use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr::NonNull;

/// The system audio object — same value used elsewhere in the codebase.
const SYSTEM_OBJECT_ID: AudioObjectID = 1;

/// Holds active CoreAudio property listeners. Listeners are unregistered and
/// the boxed proxy is reclaimed in Drop.
///
/// In practice this Drop never runs in this app because tao's
/// `EventLoop::run` diverges; the kernel cleans up at process exit. Drop is
/// implemented for correctness should the architecture ever change.
pub struct DeviceListeners {
    /// Boxed proxy whose raw pointer is held by CoreAudio as client_data.
    /// Must outlive any possible callback dispatch.
    proxy_ptr: *mut EventLoopProxyMessage,
    addresses: Vec<AudioObjectPropertyAddress>,
}

// EventLoopProxy is Send+Sync; the raw pointer points at a heap allocation
// shared with CoreAudio threads.
unsafe impl Send for DeviceListeners {}
unsafe impl Sync for DeviceListeners {}

unsafe extern "C-unwind" fn listener_proc(
    _object_id: AudioObjectID,
    num_addresses: u32,
    addresses: NonNull<AudioObjectPropertyAddress>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    let proxy = &*(client_data as *const EventLoopProxyMessage);
    let addrs = std::slice::from_raw_parts(addresses.as_ptr(), num_addresses as usize);
    let mut device_set_changed = false;
    let mut default_changed = false;
    for addr in addrs {
        if addr.mSelector == kAudioHardwarePropertyDevices {
            device_set_changed = true;
        } else if addr.mSelector == kAudioHardwarePropertyDefaultInputDevice {
            default_changed = true;
        }
    }
    if device_set_changed {
        if let Err(e) = proxy.send_event(Message::InputDeviceSetChanged) {
            warn!("Event loop closed; dropping InputDeviceSetChanged: {:?}", e);
        }
    }
    if default_changed {
        if let Err(e) = proxy.send_event(Message::DefaultInputDeviceChanged) {
            warn!("Event loop closed; dropping DefaultInputDeviceChanged: {:?}", e);
        }
    }
    0
}

impl DeviceListeners {
    pub fn install(proxy: EventLoopProxyMessage) -> Result<Self> {
        let proxy_ptr = Box::into_raw(Box::new(proxy));

        let addresses = vec![
            AudioObjectPropertyAddress {
                mSelector: kAudioHardwarePropertyDevices,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            },
            AudioObjectPropertyAddress {
                mSelector: kAudioHardwarePropertyDefaultInputDevice,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            },
        ];

        let mut installed: Vec<AudioObjectPropertyAddress> = Vec::with_capacity(addresses.len());
        for address in &addresses {
            let mut addr_copy = *address;
            let status = unsafe {
                AudioObjectAddPropertyListener(
                    SYSTEM_OBJECT_ID,
                    NonNull::new_unchecked(&mut addr_copy),
                    Some(listener_proc),
                    proxy_ptr as *mut c_void,
                )
            };
            if status != kAudioHardwareNoError {
                // Roll back partially-installed listeners and reclaim the box.
                for installed_addr in &installed {
                    let mut a = *installed_addr;
                    unsafe {
                        AudioObjectRemovePropertyListener(
                            SYSTEM_OBJECT_ID,
                            NonNull::new_unchecked(&mut a),
                            Some(listener_proc),
                            proxy_ptr as *mut c_void,
                        );
                    }
                }
                unsafe {
                    drop(Box::from_raw(proxy_ptr));
                }
                anyhow::bail!(
                    "AudioObjectAddPropertyListener for selector {:#x} failed with status {}",
                    address.mSelector,
                    status
                );
            }
            installed.push(*address);
        }

        trace!(
            "Registered {} CoreAudio property listeners",
            installed.len()
        );
        Ok(Self {
            proxy_ptr,
            addresses: installed,
        })
    }
}

impl Drop for DeviceListeners {
    fn drop(&mut self) {
        for address in &self.addresses {
            let mut addr_copy = *address;
            unsafe {
                AudioObjectRemovePropertyListener(
                    SYSTEM_OBJECT_ID,
                    NonNull::new_unchecked(&mut addr_copy),
                    Some(listener_proc),
                    self.proxy_ptr as *mut c_void,
                );
            }
        }
        // Safe: no more callbacks can fire after RemovePropertyListener returns.
        unsafe {
            drop(Box::from_raw(self.proxy_ptr));
        }
    }
}

/// Per-device property listeners. The set of devices we listen on follows
/// the MicController's tracking sets — we register a listener exactly when
/// we mute a device and unregister it when we unmute. Listeners fire
/// `Message::ExternalMuteChanged(device_id)` so the event loop can slam
/// the device back into muted state if some other app tried to unmute it.
pub struct MuteListeners {
    proxy_ptr: *mut EventLoopProxyMessage,
    /// Properties we listen on per device — both native mute and the volume
    /// scalar (so we also catch external writes that drive the volume
    /// fallback away from 0).
    addresses: Vec<AudioObjectPropertyAddress>,
    registered: HashSet<AudioObjectID>,
}

unsafe impl Send for MuteListeners {}
unsafe impl Sync for MuteListeners {}

unsafe extern "C-unwind" fn mute_listener_proc(
    object_id: AudioObjectID,
    _num_addresses: u32,
    _addresses: NonNull<AudioObjectPropertyAddress>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    let proxy = &*(client_data as *const EventLoopProxyMessage);
    let _ = proxy.send_event(Message::ExternalMuteChanged(object_id));
    0
}

impl MuteListeners {
    pub fn new(proxy: EventLoopProxyMessage) -> Self {
        let proxy_ptr = Box::into_raw(Box::new(proxy));
        let addresses = vec![
            AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyMute,
                mScope: kAudioDevicePropertyScopeInput,
                mElement: kAudioObjectPropertyElementMain,
            },
            AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyVolumeScalar,
                mScope: kAudioDevicePropertyScopeInput,
                mElement: kAudioObjectPropertyElementMain,
            },
        ];
        Self {
            proxy_ptr,
            addresses,
            registered: HashSet::new(),
        }
    }

    /// Align the set of registered device listeners with `target`.
    /// Devices in `target` that aren't yet registered get a listener.
    /// Devices currently registered but missing from `target` are removed.
    /// Skips both diffs entirely when nothing has changed since last call —
    /// the hot path on every enforce tick.
    pub fn sync(&mut self, target: &HashSet<AudioObjectID>) {
        if *target == self.registered {
            return;
        }

        let to_remove: Vec<_> = self.registered.difference(target).copied().collect();
        for id in to_remove {
            self.unregister_device(id);
        }

        let to_add: Vec<_> = target.difference(&self.registered).copied().collect();
        for id in to_add {
            self.register_device(id);
        }
    }

    fn register_device(&mut self, device_id: AudioObjectID) {
        let mut any_registered = false;
        for addr in &self.addresses {
            let mut a = *addr;
            let status = unsafe {
                AudioObjectAddPropertyListener(
                    device_id,
                    NonNull::new_unchecked(&mut a),
                    Some(mute_listener_proc),
                    self.proxy_ptr as *mut c_void,
                )
            };
            if status == kAudioHardwareNoError {
                any_registered = true;
            } else {
                // Devices commonly lack one of the two properties (e.g.
                // native-mute devices may not expose VolumeScalar on input
                // scope). Only the absence of both is a real problem.
                trace!(
                    "Listener register skipped for device {} selector {:#x}: status {}",
                    device_id, addr.mSelector, status
                );
            }
        }
        if any_registered {
            self.registered.insert(device_id);
            trace!("Registered mute listener(s) for device {}", device_id);
        } else {
            warn!(
                "Failed to register any mute/volume listener for device {}",
                device_id
            );
        }
    }

    fn unregister_device(&mut self, device_id: AudioObjectID) {
        for addr in &self.addresses {
            let mut a = *addr;
            unsafe {
                AudioObjectRemovePropertyListener(
                    device_id,
                    NonNull::new_unchecked(&mut a),
                    Some(mute_listener_proc),
                    self.proxy_ptr as *mut c_void,
                );
            }
        }
        self.registered.remove(&device_id);
        trace!("Unregistered mute listener(s) for device {}", device_id);
    }
}

impl Drop for MuteListeners {
    fn drop(&mut self) {
        let devices: Vec<_> = self.registered.iter().copied().collect();
        for id in devices {
            self.unregister_device(id);
        }
        unsafe {
            drop(Box::from_raw(self.proxy_ptr));
        }
    }
}

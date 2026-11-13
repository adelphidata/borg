//! The microphone probe on macOS: CoreAudio's process objects (macOS 14+)
//! say which processes are taking input, by bundle id. No permission
//! prompt: the list names processes, it carries no audio.
//!
//! Every `unsafe` block in the detector is in this file, and every
//! `OSStatus` is checked.

use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{bail, Result};
use objc2_core_audio::{
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningInput, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectHasProperty, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertySelector,
};
use objc2_core_foundation::{CFRetained, CFString};

use super::probe::MicProbe;

/// Reads CoreAudio's process list.
pub struct CoreAudioProbe;

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

const SYSTEM: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

impl MicProbe for CoreAudioProbe {
    fn supported(&self) -> bool {
        let mut addr = address(kAudioHardwarePropertyProcessObjectList);
        // SAFETY: `addr` is a valid, initialised property address that lives
        // for the call; the system object always exists.
        unsafe { AudioObjectHasProperty(SYSTEM, NonNull::from(&mut addr)) }
    }

    fn snapshot(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for process in process_objects()? {
            // A process that exits between the list and these reads fails
            // its reads; skip it rather than fail the whole snapshot.
            match running_input(process) {
                Ok(true) => {}
                Ok(false) | Err(_) => continue,
            }
            if let Ok(Some(bundle)) = bundle_id(process) {
                out.push(bundle);
            }
        }
        Ok(out)
    }
}

fn process_objects() -> Result<Vec<AudioObjectID>> {
    let mut addr = address(kAudioHardwarePropertyProcessObjectList);
    let mut size: u32 = 0;
    // SAFETY: valid address and out-pointer, no qualifier.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            SYSTEM,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 {
        bail!("CoreAudio process list size failed (OSStatus {status})");
    }
    let count = size as usize / std::mem::size_of::<AudioObjectID>();
    let mut ids: Vec<AudioObjectID> = vec![0; count];
    if count == 0 {
        return Ok(ids);
    }
    // SAFETY: `ids` holds `size` bytes; CoreAudio writes at most `size` and
    // reports how many it wrote back through `size`.
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(ids.as_mut_ptr().cast::<c_void>()),
        )
    };
    if status != 0 {
        bail!("CoreAudio process list failed (OSStatus {status})");
    }
    ids.truncate(size as usize / std::mem::size_of::<AudioObjectID>());
    Ok(ids)
}

fn running_input(process: AudioObjectID) -> Result<bool> {
    let mut addr = address(kAudioProcessPropertyIsRunningInput);
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `value` is a u32 the property is documented to fill.
    let status = unsafe {
        AudioObjectGetPropertyData(
            process,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    if status != 0 {
        bail!("IsRunningInput failed (OSStatus {status})");
    }
    Ok(value != 0)
}

fn bundle_id(process: AudioObjectID) -> Result<Option<String>> {
    let mut addr = address(kAudioProcessPropertyBundleID);
    let mut raw: *const CFString = std::ptr::null();
    let mut size = std::mem::size_of::<*const CFString>() as u32;
    // SAFETY: the property yields one CFStringRef, which the caller owns
    // (CoreAudio's "copy" rule for CF-typed property data).
    let status = unsafe {
        AudioObjectGetPropertyData(
            process,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut raw).cast::<c_void>(),
        )
    };
    if status != 0 {
        bail!("BundleID failed (OSStatus {status})");
    }
    let Some(ptr) = NonNull::new(raw.cast_mut()) else {
        return Ok(None);
    };
    // SAFETY: a non-null, +1 retained CFString; `CFRetained` releases it.
    let s = unsafe { CFRetained::from_raw(ptr) };
    let text = s.to_string();
    Ok((!text.is_empty()).then_some(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs against the real CoreAudio on a Mac. It cannot assert who holds
    /// the mic, only that reading the list works and yields bundle-shaped
    /// strings.
    #[test]
    fn the_process_list_is_readable_on_this_mac() {
        let probe = CoreAudioProbe;
        if !probe.supported() {
            return; // macOS 13 or older.
        }
        let bundles = probe.snapshot().expect("snapshot");
        for b in bundles {
            assert!(!b.is_empty() && !b.contains(char::is_whitespace), "{b:?}");
        }
    }
}

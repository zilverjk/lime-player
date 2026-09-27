/// A snapshot of a DAC's nominal rate and physical stream format, taken via `capture_dac_state`.
/// Compared before/after a track start (`needs_dac_prime` in `player.rs`) to detect a rate,
/// format, or route change that the HAL's own property state does not otherwise surface in time
/// for the existing before/after-`build_stream` diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct DacState {
    pub nominal_rate: u32,
    pub physical_format: String,
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{c_void, CString};
    use std::mem::size_of;
    use std::ptr::null;

    use coreaudio::audio_unit::macos_helpers::{
        get_audio_device_ids, get_hogging_pid, set_device_sample_rate, toggle_hog_mode,
    };

    const SCOPE_GLOBAL: u32 = four_cc(b"glob");
    const SCOPE_OUTPUT: u32 = four_cc(b"outp");
    const ELEMENT_MAIN: u32 = 0;
    // Per-channel elements used only when a device has no settable main-element volume
    // (`§4.5`): most such devices expose independent left/right scalars instead.
    const ELEMENT_LEFT: u32 = 1;
    const ELEMENT_RIGHT: u32 = 2;
    const PROPERTY_DEVICE_UID: u32 = four_cc(b"uid ");
    const PROPERTY_DEVICE_NOMINAL_RATE: u32 = four_cc(b"nsrt");
    const PROPERTY_DEVICE_STREAMS: u32 = four_cc(b"stm#");
    // Verified as 0x766f6c6d in objc2-core-audio `AudioHardware.rs:1334` (`§4.5`).
    const PROPERTY_VOLUME_SCALAR: u32 = four_cc(b"volm");
    const PROPERTY_STREAM_PHYSICAL_FORMAT: u32 = four_cc(b"pft ");
    const PROPERTY_STREAM_AVAILABLE_PHYSICAL_FORMATS: u32 = four_cc(b"pfta");
    const FORMAT_LINEAR_PCM: u32 = four_cc(b"lpcm");
    const FORMAT_FLAG_FLOAT: u32 = 1 << 0;
    const FORMAT_FLAG_BIG_ENDIAN: u32 = 1 << 1;
    const FORMAT_FLAG_SIGNED_INTEGER: u32 = 1 << 2;
    const FORMAT_FLAG_PACKED: u32 = 1 << 3;
    const FORMAT_FLAG_ALIGNED_HIGH: u32 = 1 << 4;
    const FORMAT_FLAG_NON_INTERLEAVED: u32 = 1 << 5;
    const STRICT_INTEGER_FLAG_MASK: u32 = FORMAT_FLAG_FLOAT
        | FORMAT_FLAG_BIG_ENDIAN
        | FORMAT_FLAG_SIGNED_INTEGER
        | FORMAT_FLAG_PACKED
        | FORMAT_FLAG_ALIGNED_HIGH
        | FORMAT_FLAG_NON_INTERLEAVED;
    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    #[repr(C)]
    struct AudioObjectPropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    #[repr(C)]
    struct AudioStreamDescription {
        sample_rate: f64,
        format_id: u32,
        format_flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        bytes_per_frame: u32,
        channels_per_frame: u32,
        bits_per_channel: u32,
        reserved: u32,
    }

    #[derive(Clone, Copy, Debug)]
    #[repr(C)]
    struct AudioValueRange {
        minimum: f64,
        maximum: f64,
    }

    #[derive(Clone, Copy, Debug)]
    #[repr(C)]
    struct AudioStreamRangedDescription {
        format: AudioStreamDescription,
        sample_rate_range: AudioValueRange,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyDataSize(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier_data: *const c_void,
            data_size: *mut u32,
        ) -> i32;
        fn AudioObjectGetPropertyData(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier_data: *const c_void,
            data_size: *mut u32,
            data: *mut c_void,
        ) -> i32;
        fn AudioObjectSetPropertyData(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier_data: *const c_void,
            data_size: u32,
            data: *const c_void,
        ) -> i32;
        fn AudioObjectHasProperty(object_id: u32, address: *const AudioObjectPropertyAddress) -> u8;
        fn AudioObjectIsPropertySettable(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            settable: *mut u8,
        ) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut i8,
            buffer_size: i64,
            encoding: u32,
        ) -> u8;
        fn CFRelease(value: *const c_void);
    }

    #[derive(Debug)]
    pub struct HogLease {
        device_id: u32,
        release_on_drop: bool,
    }

    pub fn device_id_for_uid(uid: &str) -> Result<u32, String> {
        let uid = CString::new(uid).map_err(|_| "Audio device UID contains a NUL byte".to_owned())?;
        let devices = get_audio_device_ids().map_err(|e| e.to_string())?;
        for device_id in devices {
            if get_device_uid(device_id)? == uid.to_string_lossy() {
                return Ok(device_id);
            }
        }
        Err(format!("Selected CoreAudio device is no longer available (UID {})", uid.to_string_lossy()))
    }

    pub fn acquire_hog(uid: &str) -> Result<HogLease, String> {
        let device_id = device_id_for_uid(uid)?;
        let current_pid = std::process::id() as i32;
        let owner = get_hogging_pid(device_id).map_err(|e| format!("Could not inspect Hog mode: {e}"))?;
        match owner {
            -1 => {
                let new_owner = toggle_hog_mode(device_id)
                    .map_err(|e| format!("Could not acquire Hog mode: {e}"))?;
                if new_owner != current_pid {
                    return Err(format!("Hog mode is owned by another process (pid {new_owner})"));
                }
                Ok(HogLease { device_id, release_on_drop: true })
            }
            owner if owner == current_pid => Ok(HogLease { device_id, release_on_drop: false }),
            owner => Err(format!("Hog mode is busy; another process owns this DAC (pid {owner})")),
        }
    }

    impl Drop for HogLease {
        fn drop(&mut self) {
            if !self.release_on_drop {
                return;
            }
            let current_pid = std::process::id() as i32;
            if get_hogging_pid(self.device_id).ok() == Some(current_pid) {
                match toggle_hog_mode(self.device_id) {
                    Ok(-1) => {}
                    Ok(owner) => eprintln!("Lime Player could not release Hog mode; owner is pid {owner}"),
                    Err(error) => eprintln!("Lime Player could not release Hog mode: {error}"),
                }
            }
        }
    }

    /// Which element(s) of the device carry a settable volume scalar (`§4.5`): the single main
    /// element when the device has one, otherwise the independent left/right elements when both
    /// exist and are settable, otherwise `None` (no volume control at all).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum VolumeChannels {
        Main,
        Stereo,
    }

    fn volume_address(element: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress { selector: PROPERTY_VOLUME_SCALAR, scope: SCOPE_OUTPUT, element }
    }

    fn has_property(object_id: u32, address: &AudioObjectPropertyAddress) -> bool {
        unsafe { AudioObjectHasProperty(object_id, address) != 0 }
    }

    fn is_property_settable(object_id: u32, address: &AudioObjectPropertyAddress) -> Result<bool, String> {
        let mut settable: u8 = 0;
        let status = unsafe { AudioObjectIsPropertySettable(object_id, address, &mut settable) };
        if status != 0 {
            return Err(format!("Could not check whether device volume is settable (status {status})"));
        }
        Ok(settable != 0)
    }

    fn volume_element_is_usable(device_id: u32, element: u32) -> Result<bool, String> {
        let address = volume_address(element);
        if !has_property(device_id, &address) {
            return Ok(false);
        }
        is_property_settable(device_id, &address)
    }

    fn volume_channels(device_id: u32) -> Result<Option<VolumeChannels>, String> {
        if volume_element_is_usable(device_id, ELEMENT_MAIN)? {
            return Ok(Some(VolumeChannels::Main));
        }
        if volume_element_is_usable(device_id, ELEMENT_LEFT)? && volume_element_is_usable(device_id, ELEMENT_RIGHT)? {
            return Ok(Some(VolumeChannels::Stereo));
        }
        Ok(None)
    }

    fn get_volume_scalar(device_id: u32, element: u32) -> Result<f32, String> {
        let address = volume_address(element);
        let mut value: f32 = 0.0;
        let mut data_size = size_of::<f32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &address,
                0,
                null(),
                &mut data_size,
                &mut value as *mut _ as *mut c_void,
            )
        };
        if status != 0 {
            return Err(format!("Could not read device volume (CoreAudio status {status})"));
        }
        if !value.is_finite() {
            return Err("Device reported an invalid volume value".to_owned());
        }
        Ok(value.clamp(0.0, 1.0))
    }

    fn set_volume_scalar(device_id: u32, element: u32, value: f32) -> Result<(), String> {
        let value = value.clamp(0.0, 1.0);
        let address = volume_address(element);
        let status = unsafe {
            AudioObjectSetPropertyData(
                device_id,
                &address,
                0,
                null(),
                size_of::<f32>() as u32,
                &value as *const _ as *const c_void,
            )
        };
        if status != 0 {
            return Err(format!("Could not set device volume (CoreAudio status {status})"));
        }
        Ok(())
    }

    /// Reads the device's current volume, `Ok(None)` when it has no settable volume control at
    /// all (`§4.5`). In per-channel mode the reported level is `max(l, r)`.
    pub fn device_volume(uid: &str) -> Result<Option<f32>, String> {
        let device_id = device_id_for_uid(uid)?;
        match volume_channels(device_id)? {
            None => Ok(None),
            Some(VolumeChannels::Main) => Ok(Some(get_volume_scalar(device_id, ELEMENT_MAIN)?)),
            Some(VolumeChannels::Stereo) => {
                let left = get_volume_scalar(device_id, ELEMENT_LEFT)?;
                let right = get_volume_scalar(device_id, ELEMENT_RIGHT)?;
                Ok(Some(left.max(right)))
            }
        }
    }

    /// Sets the device's volume, returning the read-back level. In per-channel mode both
    /// channels are scaled by the same ratio, preserving the device's existing balance
    /// (`scale_channel_volumes`, `§4.5`).
    pub fn set_device_volume(uid: &str, level: f32) -> Result<f32, String> {
        let level = level.clamp(0.0, 1.0);
        let device_id = device_id_for_uid(uid)?;
        let channels = volume_channels(device_id)?
            .ok_or_else(|| "Selected device has no settable volume control".to_owned())?;
        match channels {
            VolumeChannels::Main => {
                set_volume_scalar(device_id, ELEMENT_MAIN, level)?;
                get_volume_scalar(device_id, ELEMENT_MAIN)
            }
            VolumeChannels::Stereo => {
                let current_left = get_volume_scalar(device_id, ELEMENT_LEFT)?;
                let current_right = get_volume_scalar(device_id, ELEMENT_RIGHT)?;
                let (left, right) = scale_channel_volumes(current_left, current_right, level);
                set_volume_scalar(device_id, ELEMENT_LEFT, left)?;
                if let Err(error) = set_volume_scalar(device_id, ELEMENT_RIGHT, right) {
                    // The left channel already committed: restore it so a partial failure never
                    // leaves the device's balance changed (`§4.5`, `§6` Stage 4 fix).
                    let _ = set_volume_scalar(device_id, ELEMENT_LEFT, current_left);
                    return Err(error);
                }
                let new_left = get_volume_scalar(device_id, ELEMENT_LEFT)?;
                let new_right = get_volume_scalar(device_id, ELEMENT_RIGHT)?;
                Ok(new_left.max(new_right))
            }
        }
    }

    /// Scales both channels by the same ratio so a device with independent left/right volume
    /// elements keeps its existing balance when the slider sets a single target level (`§4.5`).
    /// Pure math: never touches CoreAudio, so it is safe to unit-test directly.
    pub(crate) fn scale_channel_volumes(l: f32, r: f32, target: f32) -> (f32, f32) {
        let target = target.clamp(0.0, 1.0);
        let max = l.max(r);
        if max <= f32::EPSILON {
            (target, target)
        } else {
            ((l * target / max).clamp(0.0, 1.0), (r * target / max).clamp(0.0, 1.0))
        }
    }

    pub fn set_and_verify_sample_rate(uid: &str, desired_rate: u32) -> Result<u32, String> {
        let device_id = device_id_for_uid(uid)?;
        set_device_sample_rate(device_id, desired_rate as f64)
            .map_err(|e| format!("DAC refused {desired_rate} Hz sample rate: {e}"))?;
        let nominal = get_nominal_sample_rate(device_id)?;
        if (nominal - desired_rate as f64).abs() > 0.5 {
            return Err(format!("DAC nominal rate is {nominal} Hz, not {desired_rate} Hz"));
        }
        Ok(device_id)
    }

    pub fn verify_physical_sample_rate(device_id: u32, desired_rate: u32) -> Result<(), String> {
        let actual = get_physical_format(device_id)?.sample_rate;
        if (actual - desired_rate as f64).abs() > 0.5 {
            return Err(format!("DAC physical stream is {actual} Hz, not {desired_rate} Hz"));
        }
        Ok(())
    }

    /// Polls the DAC's *physical* stream sample rate until it matches `desired_rate`, or returns
    /// an error once `timeout` elapses. `set_and_verify_sample_rate` only reads back the nominal
    /// rate, which can report the new value while the physical stream is still mid-switch; building
    /// a CPAL stream before the physical side settles is part of why a track built right after a
    /// route change (strict integer <-> float) leaves the AudioUnit configured for a stale format.
    /// Same 5 ms poll cadence as `ensure_exact_integer_output`.
    pub fn wait_for_physical_sample_rate(
        device_id: u32,
        desired_rate: u32,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        let started = std::time::Instant::now();
        loop {
            let actual = get_physical_format(device_id)?.sample_rate;
            if (actual - desired_rate as f64).abs() <= 0.5 {
                return Ok(());
            }
            if started.elapsed() >= timeout {
                return Err(format!(
                    "DAC physical stream did not settle to {desired_rate} Hz within {} ms (still {actual} Hz)",
                    timeout.as_millis()
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Human-readable snapshot of the DAC's current physical stream format (rate, bit depth,
    /// int/float, channels), for developer-only diagnostics around track handoffs.
    pub fn get_physical_format_description(device_id: u32) -> Result<String, String> {
        Ok(describe_format(&get_physical_format(device_id)?))
    }

    /// Snapshot of `uid`'s current nominal rate and physical stream format, taken independently
    /// of any pending rate/format change a track start is about to make. Used by
    /// `start_track_prepared`'s DAC-priming decision (`needs_dac_prime` in `player.rs`) to compare
    /// "before this start touched anything" against "after the new rate/format was applied".
    pub fn capture_dac_state(uid: &str) -> Result<super::DacState, String> {
        let device_id = device_id_for_uid(uid)?;
        let nominal_rate = get_nominal_sample_rate(device_id)?;
        let physical_format = get_physical_format_description(device_id)?;
        Ok(super::DacState { nominal_rate: nominal_rate.round() as u32, physical_format })
    }

    /// Select an exact signed packed 32-bit stereo format advertised by the device and verify
    /// the physical stream readback. This is a no-op when the DAC is already in the required
    /// format. Call while the Hog lease is held and before starting the output stream.
    pub fn ensure_exact_integer_output(device_id: u32, desired_rate: u32) -> Result<(), String> {
        let stream_id = output_stream_id(device_id)?;
        let current = get_stream_physical_format(stream_id)?;
        if validate_strict_integer_format(&current, desired_rate).is_ok() {
            return Ok(());
        }

        let supported = get_supported_physical_formats(stream_id)?;
        let requested = select_strict_integer_format(&supported, desired_rate).ok_or_else(|| {
            format!(
                "The selected DAC does not advertise {desired_rate} Hz little-endian packed signed 32-bit stereo LPCM; strict integer playback is unavailable."
            )
        })?;
        let address = AudioObjectPropertyAddress {
            selector: PROPERTY_STREAM_PHYSICAL_FORMAT,
            scope: SCOPE_GLOBAL,
            element: ELEMENT_MAIN,
        };
        let status = unsafe {
            AudioObjectSetPropertyData(
                stream_id,
                &address,
                0,
                null(),
                size_of::<AudioStreamDescription>() as u32,
                &requested as *const _ as *const c_void,
            )
        };
        if status != 0 {
            return Err(format!(
                "DAC advertised the required integer format but refused to select it (CoreAudio status {status})."
            ));
        }

        let started = std::time::Instant::now();
        loop {
            let actual = get_stream_physical_format(stream_id)?;
            if validate_strict_integer_format(&actual, desired_rate).is_ok() {
                return Ok(());
            }
            if started.elapsed() >= std::time::Duration::from_secs(2) {
                return Err(format!(
                    "DAC did not switch to the requested physical integer format; {}",
                    describe_format(&actual)
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn get_device_uid(device_id: u32) -> Result<String, String> {
        let address = AudioObjectPropertyAddress {
            selector: PROPERTY_DEVICE_UID,
            scope: SCOPE_GLOBAL,
            element: ELEMENT_MAIN,
        };
        let mut uid: *const c_void = null();
        let mut data_size = size_of::<*const c_void>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &address,
                0,
                null(),
                &mut data_size,
                &mut uid as *mut _ as *mut c_void,
            )
        };
        if status != 0 || uid.is_null() {
            return Err(format!("Could not read CoreAudio UID for device {device_id} (status {status})"));
        }
        let mut buffer = [0i8; 512];
        let converted = unsafe {
            CFStringGetCString(uid, buffer.as_mut_ptr(), buffer.len() as i64, CF_STRING_ENCODING_UTF8)
        };
        unsafe { CFRelease(uid); }
        if converted == 0 {
            return Err(format!("Could not convert CoreAudio UID for device {device_id}"));
        }
        let bytes: Vec<u8> = buffer.iter().take_while(|byte| **byte != 0).map(|byte| *byte as u8).collect();
        String::from_utf8(bytes).map_err(|e| e.to_string())
    }

    fn get_nominal_sample_rate(device_id: u32) -> Result<f64, String> {
        let address = AudioObjectPropertyAddress {
            selector: PROPERTY_DEVICE_NOMINAL_RATE,
            scope: SCOPE_GLOBAL,
            element: ELEMENT_MAIN,
        };
        let mut rate = 0.0f64;
        let mut data_size = size_of::<f64>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &address,
                0,
                null(),
                &mut data_size,
                &mut rate as *mut _ as *mut c_void,
            )
        };
        if status != 0 {
            return Err(format!("Could not read DAC nominal sample rate (CoreAudio status {status})"));
        }
        Ok(rate)
    }

    fn output_stream_id(device_id: u32) -> Result<u32, String> {
        let streams_address = AudioObjectPropertyAddress {
            selector: PROPERTY_DEVICE_STREAMS,
            scope: SCOPE_OUTPUT,
            element: ELEMENT_MAIN,
        };
        let mut bytes = 0u32;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(device_id, &streams_address, 0, null(), &mut bytes)
        };
        if status != 0 || bytes < size_of::<u32>() as u32 {
            return Err(format!("Could not inspect DAC output stream (CoreAudio status {status})"));
        }
        let mut stream_ids = vec![0u32; bytes as usize / size_of::<u32>()];
        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &streams_address,
                0,
                null(),
                &mut bytes,
                stream_ids.as_mut_ptr() as *mut c_void,
            )
        };
        if status != 0 {
            return Err(format!("Could not read DAC output streams (CoreAudio status {status})"));
        }
        stream_ids.into_iter().find(|id| *id != 0)
            .ok_or_else(|| "Selected DAC exposes no output stream".to_owned())
    }

    fn get_physical_format(device_id: u32) -> Result<AudioStreamDescription, String> {
        get_stream_physical_format(output_stream_id(device_id)?)
    }

    fn get_stream_physical_format(stream_id: u32) -> Result<AudioStreamDescription, String> {
        let format_address = AudioObjectPropertyAddress {
            selector: PROPERTY_STREAM_PHYSICAL_FORMAT,
            scope: SCOPE_GLOBAL,
            element: ELEMENT_MAIN,
        };
        let mut format = AudioStreamDescription {
            sample_rate: 0.0,
            format_id: 0,
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: 0,
            bytes_per_frame: 0,
            channels_per_frame: 0,
            bits_per_channel: 0,
            reserved: 0,
        };
        let mut data_size = size_of::<AudioStreamDescription>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                stream_id,
                &format_address,
                0,
                null(),
                &mut data_size,
                &mut format as *mut _ as *mut c_void,
            )
        };
        if status != 0 {
            return Err(format!("Could not read DAC physical stream format (CoreAudio status {status})"));
        }
        if !format.sample_rate.is_finite() || format.sample_rate <= 0.0 {
            return Err("DAC reported an invalid physical stream sample rate".to_owned());
        }
        Ok(format)
    }

    fn get_supported_physical_formats(
        stream_id: u32,
    ) -> Result<Vec<AudioStreamRangedDescription>, String> {
        let address = AudioObjectPropertyAddress {
            selector: PROPERTY_STREAM_AVAILABLE_PHYSICAL_FORMATS,
            scope: SCOPE_GLOBAL,
            element: ELEMENT_MAIN,
        };
        let mut data_size = 0u32;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(stream_id, &address, 0, null(), &mut data_size)
        };
        if status != 0 {
            return Err(format!(
                "Could not inspect DAC-supported physical formats (CoreAudio status {status})"
            ));
        }
        let format_size = size_of::<AudioStreamRangedDescription>();
        if data_size == 0 || data_size as usize % format_size != 0 {
            return Err("DAC returned an invalid physical-format list".to_owned());
        }
        let count = data_size as usize / format_size;
        let empty_format = AudioStreamDescription {
            sample_rate: 0.0,
            format_id: 0,
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: 0,
            bytes_per_frame: 0,
            channels_per_frame: 0,
            bits_per_channel: 0,
            reserved: 0,
        };
        let empty_range = AudioValueRange { minimum: 0.0, maximum: 0.0 };
        let mut formats = vec![AudioStreamRangedDescription {
            format: empty_format,
            sample_rate_range: empty_range,
        }; count];
        let status = unsafe {
            AudioObjectGetPropertyData(
                stream_id,
                &address,
                0,
                null(),
                &mut data_size,
                formats.as_mut_ptr() as *mut c_void,
            )
        };
        if status != 0 {
            return Err(format!(
                "Could not read DAC-supported physical formats (CoreAudio status {status})"
            ));
        }
        Ok(formats)
    }

    fn select_strict_integer_format(
        formats: &[AudioStreamRangedDescription],
        desired_rate: u32,
    ) -> Option<AudioStreamDescription> {
        let desired_rate_f64 = desired_rate as f64;
        formats.iter().find_map(|ranged| {
            let range_supports_rate = desired_rate_f64 >= ranged.sample_rate_range.minimum
                && desired_rate_f64 <= ranged.sample_rate_range.maximum;
            let exact_asbd_rate = (ranged.format.sample_rate - desired_rate_f64).abs() <= 0.5;
            if !range_supports_rate && !exact_asbd_rate {
                return None;
            }
            let mut format = ranged.format;
            format.sample_rate = desired_rate_f64;
            validate_strict_integer_format(&format, desired_rate).ok()?;
            Some(format)
        })
    }

    fn validate_strict_integer_format(format: &AudioStreamDescription, desired_rate: u32) -> Result<(), String> {
        let expected_flags = FORMAT_FLAG_SIGNED_INTEGER | FORMAT_FLAG_PACKED;
        let flags_match = format.format_flags & STRICT_INTEGER_FLAG_MASK == expected_flags;
        let matches = (format.sample_rate - desired_rate as f64).abs() <= 0.5
            && format.format_id == FORMAT_LINEAR_PCM
            && flags_match
            && format.bits_per_channel == 32
            && format.bytes_per_frame == 8
            && format.channels_per_frame == 2
            && format.frames_per_packet == 1
            && format.bytes_per_packet == 8;
        if matches {
            Ok(())
        } else {
            Err(format!(
                "Strict integer PCM requires {desired_rate} Hz, little-endian packed signed 32-bit stereo LPCM (8 bytes/frame); DAC reports {}",
                describe_format(format),
            ))
        }
    }

    fn describe_format(format: &AudioStreamDescription) -> String {
        format!(
            "rate={} format={:#010x} flags={:#010x} bits={} bytes/frame={} channels={} frames/packet={} bytes/packet={}",
            format.sample_rate,
            format.format_id,
            format.format_flags,
            format.bits_per_channel,
            format.bytes_per_frame,
            format.channels_per_frame,
            format.frames_per_packet,
            format.bytes_per_packet,
        )
    }

    const fn four_cc(value: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*value)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn selectors_match_coreaudio_fourcc_values() {
            assert_eq!(PROPERTY_DEVICE_UID, u32::from_be_bytes(*b"uid "));
            assert_eq!(PROPERTY_DEVICE_NOMINAL_RATE, u32::from_be_bytes(*b"nsrt"));
            assert_eq!(PROPERTY_STREAM_PHYSICAL_FORMAT, u32::from_be_bytes(*b"pft "));
            assert_eq!(PROPERTY_STREAM_AVAILABLE_PHYSICAL_FORMATS, u32::from_be_bytes(*b"pfta"));
        }

        fn strict_i32_format(rate: f64) -> AudioStreamDescription {
            AudioStreamDescription {
                sample_rate: rate,
                format_id: FORMAT_LINEAR_PCM,
                format_flags: FORMAT_FLAG_SIGNED_INTEGER | FORMAT_FLAG_PACKED,
                bytes_per_packet: 8,
                frames_per_packet: 1,
                bytes_per_frame: 8,
                channels_per_frame: 2,
                bits_per_channel: 32,
                reserved: 0,
            }
        }

        #[test]
        fn accepts_matching_physical_i32_pcm_format() {
            assert!(validate_strict_integer_format(&strict_i32_format(96_000.0), 96_000).is_ok());
        }

        #[test]
        fn refuses_non_exact_physical_output_formats() {
            let good = strict_i32_format(96_000.0);
            let mut mismatches = Vec::new();

            let mut format_id = good;
            format_id.format_id = four_cc(b"lpcm").wrapping_add(1);
            mismatches.push(format_id);

            let mut float = good;
            float.format_flags = FORMAT_FLAG_FLOAT | FORMAT_FLAG_PACKED;
            mismatches.push(float);

            let mut big_endian = good;
            big_endian.format_flags |= FORMAT_FLAG_BIG_ENDIAN;
            mismatches.push(big_endian);

            let mut wrong_depth = good;
            wrong_depth.bits_per_channel = 24;
            mismatches.push(wrong_depth);

            let mut wrong_layout = good;
            wrong_layout.bytes_per_frame = 6;
            mismatches.push(wrong_layout);

            let mut wrong_rate = good;
            wrong_rate.sample_rate = 44_100.0;
            mismatches.push(wrong_rate);

            for format in mismatches {
                assert!(validate_strict_integer_format(&format, 96_000).is_err());
            }
        }

        fn ranged(format: AudioStreamDescription, minimum: f64, maximum: f64) -> AudioStreamRangedDescription {
            AudioStreamRangedDescription {
                format,
                sample_rate_range: AudioValueRange { minimum, maximum },
            }
        }

        #[test]
        fn selects_only_an_advertised_exact_integer_format_for_requested_rate() {
            let mut float = strict_i32_format(0.0);
            float.format_flags = FORMAT_FLAG_FLOAT | FORMAT_FLAG_PACKED;
            let supported = [
                ranged(float, 44_100.0, 384_000.0),
                ranged(strict_i32_format(0.0), 48_000.0, 384_000.0),
                ranged(strict_i32_format(0.0), 44_100.0, 384_000.0),
            ];

            let selected = select_strict_integer_format(&supported, 44_100).unwrap();
            assert_eq!(selected.sample_rate, 44_100.0);
            assert!(validate_strict_integer_format(&selected, 44_100).is_ok());
        }

        #[test]
        fn refuses_an_integer_format_when_requested_rate_is_not_advertised() {
            let supported = [ranged(strict_i32_format(0.0), 48_000.0, 384_000.0)];
            assert!(select_strict_integer_format(&supported, 44_100).is_none());
        }

        #[test]
        fn scale_channel_volumes_preserves_balance() {
            assert_eq!(scale_channel_volumes(0.8, 0.4, 0.5), (0.5, 0.25));
            assert_eq!(scale_channel_volumes(0.0, 0.0, 0.3), (0.3, 0.3));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    #[derive(Debug)]
    pub struct HogLease;

    pub fn device_id_for_uid(_uid: &str) -> Result<u32, String> {
        Err("CoreAudio device identifiers are available only on macOS".into())
    }

    pub fn acquire_hog(_uid: &str) -> Result<HogLease, String> {
        Err("Hog mode is available only on macOS".into())
    }

    pub fn set_and_verify_sample_rate(_uid: &str, _desired_rate: u32) -> Result<u32, String> {
        Err("Per-device sample-rate control is available only on macOS".into())
    }

    pub fn verify_physical_sample_rate(_device_id: u32, _desired_rate: u32) -> Result<(), String> {
        Err("Physical sample-rate readback is available only on macOS".into())
    }

    pub fn wait_for_physical_sample_rate(
        _device_id: u32,
        _desired_rate: u32,
        _timeout: std::time::Duration,
    ) -> Result<(), String> {
        Err("Physical sample-rate readback is available only on macOS".into())
    }

    pub fn get_physical_format_description(_device_id: u32) -> Result<String, String> {
        Err("Physical format inspection is available only on macOS".into())
    }

    pub fn ensure_exact_integer_output(_device_id: u32, _desired_rate: u32) -> Result<(), String> {
        Err("Exact physical integer output selection is available only on macOS".into())
    }

    pub fn capture_dac_state(_uid: &str) -> Result<super::DacState, String> {
        Err("DAC physical-state capture is available only on macOS".into())
    }

    pub fn device_volume(_uid: &str) -> Result<Option<f32>, String> {
        Err("Device volume control is available only on macOS.".into())
    }

    pub fn set_device_volume(_uid: &str, _level: f32) -> Result<f32, String> {
        Err("Device volume control is available only on macOS.".into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn stubs_refuse_device_volume_control() {
            assert!(device_volume("any-uid").is_err());
            assert!(set_device_volume("any-uid", 0.5).is_err());
        }

        #[test]
        fn stub_refuses_dac_state_capture() {
            assert!(capture_dac_state("any-uid").is_err());
        }
    }
}

pub use platform::{
    HogLease, acquire_hog, capture_dac_state, device_volume, ensure_exact_integer_output,
    get_physical_format_description, set_and_verify_sample_rate, set_device_volume, verify_physical_sample_rate,
    wait_for_physical_sample_rate,
};

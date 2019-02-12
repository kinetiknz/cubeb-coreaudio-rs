// Copyright © 2018 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details.

extern crate coreaudio_sys;
extern crate libc;

mod auto_array;
mod auto_release;
mod dispatch_utils;
mod utils;
mod owned_critical_section;

// cubeb_backend::{*} is referred:
// - ffi                : cubeb_sys::*                      (cubeb-core/lib.rs).
// - Context            : pub struct Context                (cubeb-core/context.rs).
// - ContextOps         : pub trait ContextOps              (cubeb-backend/trait.rs).
// - DeviceCollectionRef: pub struct DeviceCollectionRef    (cubeb-core/device_collection.rs).
// - DeviceId           : pub type DeviceId                 (cubeb-core/device.rs).
// - DeviceType         : pub struct DeviceType             (cubeb-core/device.rs).
// - Error              : pub struct Error                  (cubeb-core/error.rs).
// - Ops                : pub struct Ops                    (cubeb-backend/ops.rs).
// - Result             : pub type Result<T>                (cubeb-core/error.rs).
// - Stream             : pub struct Stream                 (cubeb-core/stream.rs)
// - StreamOps          : pub trait StreamOps               (cubeb-backend/traits.rs)
// - StreamParams       : pub struct StreamParams           (cubeb-core/stream.rs)
// - StreamParamsRef    : pub struct StreamParamsRef        (cubeb-core/stream.rs)
use atomic;
use cubeb_backend::{ffi, Context, ContextOps, DeviceCollectionRef, DeviceId,
                    DeviceRef, DeviceType, Error, Ops, Result, SampleFormat,
                    Stream, StreamOps, StreamParams, StreamParamsRef,
                    StreamPrefs};
use self::auto_array::*;
use self::auto_release::*;
use self::dispatch_utils::*;
use self::coreaudio_sys::*;
use self::utils::*;
use self::owned_critical_section::*;
use std::cmp;
use std::ffi::{CStr, CString};
use std::mem;
use std::os::raw::{c_void, c_char};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

// TODO:
// 1. We use AudioDeviceID and AudioObjectID at the same time.
//    They are actually same. Maybe it's better to use only one
//    of them so code reader don't get confused about their types.
// 2. Maybe we can merge `io_side` and `DeviceType`.
// 3. Add assertions like:
//    `assert!(devtype == DeviceType::INPUT || devtype == DeviceType::OUTPUT)`
//    if the function is only called for either input or output. Then
//    `if (devtype == DeviceType::INPUT) { ... } else { ... }`
//    makes sense. In fact, for those variables depends on DeviceType, we can
//    implement a `From` trait to get them.

const NO_ERR: OSStatus = 0;

const AU_OUT_BUS: AudioUnitElement = 0;
const AU_IN_BUS: AudioUnitElement = 1;

const DISPATCH_QUEUE_LABEL: &'static str = "org.mozilla.cubeb";
const PRIVATE_AGGREGATE_DEVICE_NAME: &'static str = "CubebAggregateDevice";

// A compile-time static string mapped to kAudioAggregateDeviceNameKey
// https://github.com/phracker/MacOSX-SDKs/blob/9fc3ed0ad0345950ac25c28695b0427846eea966/MacOSX10.12.sdk/System/Library/Frameworks/CoreAudio.framework/Versions/A/Headers/AudioHardware.h#L1513
const AGGREGATE_DEVICE_NAME_KEY: &'static str = "name";

// A compile-time static string mapped to kAudioAggregateDeviceUIDKey
// https://github.com/phracker/MacOSX-SDKs/blob/9fc3ed0ad0345950ac25c28695b0427846eea966/MacOSX10.12.sdk/System/Library/Frameworks/CoreAudio.framework/Versions/A/Headers/AudioHardware.h#L1505
const AGGREGATE_DEVICE_UID: &'static str = "uid";

// A compile-time static string mapped to kAudioAggregateDeviceIsPrivateKey
// https://github.com/phracker/MacOSX-SDKs/blob/9fc3ed0ad0345950ac25c28695b0427846eea966/MacOSX10.12.sdk/System/Library/Frameworks/CoreAudio.framework/Versions/A/Headers/AudioHardware.h#L1553
const AGGREGATE_DEVICE_PRIVATE_KEY: &'static str = "private";

// A compile-time static string mapped to kAudioAggregateDeviceIsStackedKey
// https://github.com/phracker/MacOSX-SDKs/blob/9fc3ed0ad0345950ac25c28695b0427846eea966/MacOSX10.12.sdk/System/Library/Frameworks/CoreAudio.framework/Versions/A/Headers/AudioHardware.h#L1562
const AGGREGATE_DEVICE_STACKED_KEY: &'static str = "stacked";

/* Testing empirically, some headsets report a minimal latency that is very
 * low, but this does not work in practice. Lie and say the minimum is 256
 * frames. */
const SAFE_MIN_LATENCY_FRAMES: u32 = 256;
const SAFE_MAX_LATENCY_FRAMES: u32 = 512;

// TODO: Move them into a seperate module, or add an API to generate these
//       property addressed.
const DEFAULT_INPUT_DEVICE_PROPERTY_ADDRESS: AudioObjectPropertyAddress =
    AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultInputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };

const DEFAULT_OUTPUT_DEVICE_PROPERTY_ADDRESS: AudioObjectPropertyAddress =
    AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultOutputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
};

const DEVICE_IS_ALIVE_PROPERTY_ADDRESS: AudioObjectPropertyAddress =
    AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceIsAlive,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
};

const DEVICES_PROPERTY_ADDRESS: AudioObjectPropertyAddress =
    AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
};

const INPUT_DATA_SOURCE_PROPERTY_ADDRESS: AudioObjectPropertyAddress =
    AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDataSource,
        mScope: kAudioDevicePropertyScopeInput,
        mElement: kAudioObjectPropertyElementMaster,
};

const OUTPUT_DATA_SOURCE_PROPERTY_ADDRESS: AudioObjectPropertyAddress =
    AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDataSource,
        mScope: kAudioDevicePropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMaster,
};

bitflags! {
    struct device_flags: u32 {
        const DEV_UNKNOWN           = 0b00000000; /* Unknown */
        const DEV_INPUT             = 0b00000001; /* Record device like mic */
        const DEV_OUTPUT            = 0b00000010; /* Playback device like speakers */
        const DEV_SYSTEM_DEFAULT    = 0b00000100; /* System default device */
        const DEV_SELECTED_DEFAULT  = 0b00001000; /* User selected to use the system default device */
    }
}

#[derive(Debug, PartialEq)]
enum io_side {
  INPUT,
  OUTPUT,
}

fn to_string(side: &io_side) -> &'static str
{
    match side {
        io_side::INPUT => "input",
        io_side::OUTPUT => "output"
    }
}

#[derive(Clone, Debug)]
struct device_info {
    id: AudioDeviceID,
    flags: device_flags
}

impl device_info {
    fn new() -> Self {
        device_info {
            id: kAudioObjectUnknown,
            flags: device_flags::DEV_UNKNOWN,
        }
    }
}

impl Default for device_info {
    fn default() -> Self {
        unsafe { mem::zeroed() }
    }
}

// Since we need to add `property_listener` as one of the members of
// `AudioUnitStream`, so we store `stream` as a raw pointer to avoid
// the `lifetime` check issues for structs that mutual references rather
// than store it as reference with lifetime.
// TODO: A safer way to do is
// 1. either we use Rc<RefCell<AudioUnitStream<'ctx>>> for `stream`
//    to get run-time check for this nullable pointer
// 2. or refactor the code to avoid the mutual references and guarantee
//    the `stream` is alive when `property_listener` is called.
#[derive(Debug)]
struct property_listener<'ctx> {
    device_id: AudioDeviceID,
    property_address: &'static AudioObjectPropertyAddress,
    callback: audio_object_property_listener_proc,
    stream: *mut AudioUnitStream<'ctx>,
}

impl<'ctx> property_listener<'ctx> {
    fn new(id: AudioDeviceID,
           address: &'static AudioObjectPropertyAddress,
           listener: audio_object_property_listener_proc,
           stm: *mut AudioUnitStream<'ctx>) -> Self {
        property_listener {
            device_id: id,
            property_address: address,
            callback: listener,
            stream: stm
        }
    }
}

fn has_input(stm: &AudioUnitStream) -> bool
{
    stm.input_stream_params.rate() > 0
}

fn has_output(stm: &AudioUnitStream) -> bool
{
    stm.output_stream_params.rate() > 0
}

fn audiounit_increment_active_streams(ctx: &mut AudioUnitContext)
{
    ctx.mutex.assert_current_thread_owns();
    ctx.active_streams += 1;
}

fn audiounit_decrement_active_streams(ctx: &mut AudioUnitContext)
{
    ctx.mutex.assert_current_thread_owns();
    ctx.active_streams -= 1;
}

fn audiounit_active_streams(ctx: &mut AudioUnitContext) -> i32
{
    ctx.mutex.assert_current_thread_owns();
    ctx.active_streams
}

fn audiounit_set_global_latency(ctx: &mut AudioUnitContext, latency_frames: u32)
{
    ctx.mutex.assert_current_thread_owns();
    assert_eq!(audiounit_active_streams(ctx), 1);
    ctx.global_latency_frames = latency_frames;
}

fn audiounit_make_silent(ioData: &mut AudioBuffer) {
    assert!(!ioData.mData.is_null());
    unsafe {
        libc::memset(ioData.mData, 0, ioData.mDataByteSize as usize);
    }
}

fn audiounit_render_input(stm: &mut AudioUnitStream,
                          flags: *mut AudioUnitRenderActionFlags,
                          tstamp: *const AudioTimeStamp,
                          bus: u32,
                          input_frames: u32) -> OSStatus
{
    /* Create the AudioBufferList to store input. */
    let mut input_buffer_list = AudioBufferList::default();
    input_buffer_list.mBuffers[0].mDataByteSize = stm.input_desc.mBytesPerFrame * input_frames;
    input_buffer_list.mBuffers[0].mData = ptr::null_mut();
    input_buffer_list.mBuffers[0].mNumberChannels = stm.input_desc.mChannelsPerFrame;
    input_buffer_list.mNumberBuffers = 1;

    assert!(!stm.input_unit.is_null());
    let r = audio_unit_render(stm.input_unit,
                              flags,
                              tstamp,
                              bus,
                              input_frames,
                              &mut input_buffer_list);

    if r != NO_ERR {
        cubeb_log!("AudioUnitRender rv={}", r);
        if r != kAudioUnitErr_CannotDoInCurrentContext {
            return r;
        }
        if !stm.output_unit.is_null() {
            // kAudioUnitErr_CannotDoInCurrentContext is returned when using a BT
            // headset and the profile is changed from A2DP to HFP/HSP. The previous
            // output device is no longer valid and must be reset.
            audiounit_reinit_stream_async(stm, device_flags::DEV_INPUT | device_flags::DEV_OUTPUT);
        }
        // For now state that no error occurred and feed silence, stream will be
        // resumed once reinit has completed.
        cubeb_logv!("({:p}) input: reinit pending feeding silence instead", stm);
        stm.input_linear_buffer.as_mut().unwrap().push_zeros((input_frames * stm.input_desc.mChannelsPerFrame) as usize);
    } else {
        /* Copy input data in linear buffer. */
        stm.input_linear_buffer.as_mut().unwrap().push(input_buffer_list.mBuffers[0].mData,
                                                       (input_frames * stm.input_desc.mChannelsPerFrame) as usize);
    }

    /* Advance input frame counter. */
    assert!(input_frames > 0);
    *stm.frames_read.get_mut() += input_frames as i64;

    cubeb_logv!("({:p}) input: buffers {}, size {}, channels {}, rendered frames {}, total frames {}.",
                stm,
                input_buffer_list.mNumberBuffers,
                input_buffer_list.mBuffers[0].mDataByteSize,
                input_buffer_list.mBuffers[0].mNumberChannels,
                input_frames,
                stm.input_linear_buffer.as_ref().unwrap().elements() / stm.input_desc.mChannelsPerFrame as usize);

    NO_ERR
}

extern fn audiounit_input_callback(user_ptr: *mut c_void,
                                   flags: *mut AudioUnitRenderActionFlags,
                                   tstamp: *const AudioTimeStamp,
                                   bus: u32,
                                   input_frames: u32,
                                   _: *mut AudioBufferList) -> OSStatus
{
    let stm = unsafe { &mut *(user_ptr as *mut AudioUnitStream) };

    assert!(!stm.input_unit.is_null());
    assert_eq!(bus, AU_IN_BUS);

    if *stm.shutdown.get_mut() {
        cubeb_log!("({:p}) input shutdown", stm);
        return NO_ERR;
    }

    let r = audiounit_render_input(stm, flags, tstamp, bus, input_frames);
    if r != NO_ERR {
        return r;
    }

    // Full Duplex. We'll call data_callback in the AudioUnit output callback.
    if !stm.output_unit.is_null() {
        return NO_ERR;
    }

    /* Input only. Call the user callback through resampler.
       Resampler will deliver input buffer in the correct rate. */
    assert!(input_frames as usize <= stm.input_linear_buffer.as_ref().unwrap().elements() / stm.input_desc.mChannelsPerFrame as usize);
    let mut total_input_frames = (stm.input_linear_buffer.as_ref().unwrap().elements() / stm.input_desc.mChannelsPerFrame as usize) as i64;
    assert!(!stm.resampler.as_mut_ptr().is_null());
    assert!(!stm.input_linear_buffer.as_ref().unwrap().as_ptr().is_null());
    let outframes = unsafe {
        ffi::cubeb_resampler_fill(stm.resampler.as_mut_ptr(),
                                  stm.input_linear_buffer.as_mut().unwrap().as_mut_ptr(),
                                  &mut total_input_frames,
                                  ptr::null_mut(),
                                  0)
    };
    if outframes < total_input_frames {
        assert_eq!(audio_output_unit_stop(stm.input_unit), NO_ERR);

        // TODO: C version doesn't check if state_callback is a null pointer.
        if stm.state_callback.is_some() {
            unsafe {
                (stm.state_callback.unwrap())(
                    stm as *mut AudioUnitStream as *mut ffi::cubeb_stream,
                    stm.user_ptr,
                    ffi::CUBEB_STATE_DRAINED);
            }
        }

        return NO_ERR;
    }

    // Reset input buffer
    stm.input_linear_buffer.as_mut().unwrap().clear();

    NO_ERR
}

extern fn audiounit_output_callback(user_ptr: *mut c_void,
                                    _: *mut AudioUnitRenderActionFlags,
                                    tstamp: *const AudioTimeStamp,
                                    bus: u32,
                                    output_frames: u32,
                                    outBufferList: *mut AudioBufferList) -> OSStatus
{
    assert_eq!(bus, AU_OUT_BUS);
    assert_eq!(unsafe { (&(*outBufferList)).mNumberBuffers }, 1);

    let stm = unsafe { &mut *(user_ptr as *mut AudioUnitStream) };
    let buffers = unsafe {
        let ptr = (&mut (*outBufferList)).mBuffers.as_mut_ptr();
        let len = (&(*outBufferList)).mNumberBuffers as usize;
        slice::from_raw_parts_mut(ptr, len)
    };

    // TODO: Why don't we replace `has_input(stm)` by `stm.input_linear_buffer.is_some()` ?
    cubeb_logv!("({:p}) output: buffers {}, size {}, channels {}, frames {}, total input frames {}.",
                stm,
                buffers.len(),
                buffers[0].mDataByteSize,
                buffers[0].mNumberChannels,
                output_frames,
                if has_input(stm) { stm.input_linear_buffer.as_ref().unwrap().elements() / stm.input_desc.mChannelsPerFrame as usize } else { 0 });

    NO_ERR
}

fn audiounit_set_device_info(stm: &mut AudioUnitStream, id: AudioDeviceID, devtype: DeviceType) -> Result<()>
{
    assert!(devtype == DeviceType::INPUT || devtype == DeviceType::OUTPUT);

    let info = if devtype == DeviceType::INPUT {
        &mut stm.input_device
    } else {
        &mut stm.output_device
    };

    *info = device_info::default();
    info.id = id;
    info.flags |= if devtype == DeviceType::INPUT {
        device_flags::DEV_INPUT
    } else {
        device_flags::DEV_OUTPUT
    };

    let default_device_id = audiounit_get_default_device_id(devtype);
    if default_device_id == kAudioObjectUnknown {
        return Err(Error::error());
    }

    if id == kAudioObjectUnknown {
        info.id = default_device_id;
        info.flags |= device_flags::DEV_SELECTED_DEFAULT;
    }

    if info.id == default_device_id {
        info.flags |= device_flags::DEV_SYSTEM_DEFAULT;
    }

    assert_ne!(info.id, kAudioObjectUnknown);
    assert!(info.flags.contains(device_flags::DEV_INPUT) && !info.flags.contains(device_flags::DEV_OUTPUT) ||
            !info.flags.contains(device_flags::DEV_INPUT) && info.flags.contains(device_flags::DEV_OUTPUT));

    Ok(())
}

fn audiounit_reinit_stream_async(stm: &mut AudioUnitStream, flags: device_flags)
{
    if stm.reinit_pending.swap(true, Ordering::SeqCst) {
        // A reinit task is already pending, nothing more to do.
        // TODO: redundant space! Sync with C version.
        cubeb_log!("({:p}) re-init stream task already pending, cancelling request ", stm);
        return;
    }

    // Rust compilter doesn't allow a pointer to be passed across threads.
    // A hacky way to do that is to cast the pointer into a value, then
    // the value, which is actually an address, can be copied into threads.
    let stm_ptr = stm as *mut AudioUnitStream as usize;
    // Use a new thread, through the queue, to avoid deadlock when calling
    // Get/SetProperties method from inside notify callback
    async_dispatch(stm.context.serial_queue, move || {
        let stm = unsafe { &mut *(stm_ptr as *mut AudioUnitStream) };
        if *stm.destroy_pending.get_mut() {
            cubeb_log!("({:p}) stream pending destroy, cancelling reinit task", stm);
            return;
        }

        // TODO: Reinit stream ...

        *stm.switching_device.get_mut() = false;
        *stm.reinit_pending.get_mut() = false;
    });
}

fn event_addr_to_string(selector: AudioObjectPropertySelector) -> &'static str
{
    match selector {
        coreaudio_sys::kAudioHardwarePropertyDefaultOutputDevice =>
            "kAudioHardwarePropertyDefaultOutputDevice",
        coreaudio_sys::kAudioHardwarePropertyDefaultInputDevice =>
            "kAudioHardwarePropertyDefaultInputDevice",
        coreaudio_sys::kAudioDevicePropertyDeviceIsAlive =>
            "kAudioDevicePropertyDeviceIsAlive",
        coreaudio_sys::kAudioDevicePropertyDataSource =>
            "kAudioDevicePropertyDataSource",
        _ => "Unknown"
    }
}

extern fn audiounit_property_listener_callback(id: AudioObjectID, address_count: u32,
                                               addresses: *const AudioObjectPropertyAddress,
                                               user: *mut c_void) -> OSStatus
{
    let stm = unsafe { &mut *(user as *mut AudioUnitStream) };
    let addrs = unsafe { slice::from_raw_parts(addresses, address_count as usize) };
    if *stm.switching_device.get_mut() {
        cubeb_log!("Switching is already taking place. Skip Event {} for id={}", event_addr_to_string(addrs[0].mSelector), id);
        return 0;
    }
    *stm.switching_device.get_mut() = true;

    cubeb_log!("({:p}) Audio device changed, {} events.", stm, address_count);
    for (i, addr) in addrs.iter().enumerate() {
        match addr.mSelector {
            coreaudio_sys::kAudioHardwarePropertyDefaultOutputDevice => {
                cubeb_log!("Event[{}] - mSelector == kAudioHardwarePropertyDefaultOutputDevice for id={}", i, id);
            },
            coreaudio_sys::kAudioHardwarePropertyDefaultInputDevice => {
                cubeb_log!("Event[{}] - mSelector == kAudioHardwarePropertyDefaultInputDevice for id={}", i, id);
            },
            coreaudio_sys::kAudioDevicePropertyDeviceIsAlive => {
                cubeb_log!("Event[{}] - mSelector == kAudioDevicePropertyDeviceIsAlive for id={}", i, id);
                // If this is the default input device ignore the event,
                // kAudioHardwarePropertyDefaultInputDevice will take care of the switch
                if stm.input_device.flags.contains(device_flags::DEV_SYSTEM_DEFAULT) {
                    cubeb_log!("It's the default input device, ignore the event");
                    *stm.switching_device.get_mut() = false;
                    return 0;
                }
            },
            coreaudio_sys::kAudioDevicePropertyDataSource => {
                // TODO: Why we use kAudioHardwarePropertyDataSource instead of kAudioDevicePropertyDataSource ?
                cubeb_log!("Event[{}] - mSelector == kAudioHardwarePropertyDataSource for id={}", i, id);
            },
            _ => {
                cubeb_log!("Event[{}] - mSelector == Unexpected Event id {}, return", i, addr.mSelector);
                *stm.switching_device.get_mut() = false;
                return 0;
            }
        }
    }

    // Allow restart to choose the new default
    let mut switch_side = device_flags::DEV_UNKNOWN;
    if has_input(stm) {
        switch_side |= device_flags::DEV_INPUT;
    }
    if has_output(stm) {
        switch_side |= device_flags::DEV_OUTPUT;
    }
    // TODO: Assert it's either input or output here ?
    //       or early return if it's not input and it's not output ?

    for addr in addrs.iter() {
        // TODO: Since match only use `_` here, why don't we remove the match ?
        //       It will be called anyway (Sync with C version).
        match addr.mSelector {
            // If addr.mSelector is not
            // kAudioHardwarePropertyDefaultOutputDevice or
            // kAudioHardwarePropertyDefaultInputDevice or
            // kAudioDevicePropertyDeviceIsAlive or
            // kAudioDevicePropertyDataSource
            // then this function will early return in the match block above.
            _ => {
                // The scope of `_dev_cb_lock` is a critical section.
                let _dev_cb_lock = AutoLock::new(&mut stm.device_changed_callback_lock);
                if let Some(device_changed_callback) = stm.device_changed_callback {
                    unsafe { device_changed_callback(stm.user_ptr); }
                }
            }
        }
    }

    audiounit_reinit_stream_async(stm, switch_side);

    0
}

fn audiounit_add_listener(listener: &property_listener) -> OSStatus
{
    audio_object_add_property_listener(listener.device_id,
                                       listener.property_address,
                                       listener.callback,
                                       listener.stream as *mut c_void)
}

fn audiounit_remove_listener(listener: &property_listener) -> OSStatus
{
    audio_object_remove_property_listener(listener.device_id,
                                          listener.property_address,
                                          listener.callback,
                                          listener.stream as *mut c_void)
}

fn audiounit_install_device_changed_callback(stm: &mut AudioUnitStream) -> Result<()>
{
    let mut rv = NO_ERR;
    let mut r = Ok(());

    if !stm.output_unit.is_null() {
        /* This event will notify us when the data source on the same device changes,
         * for example when the user plugs in a normal (non-usb) headset in the
         * headphone jack. */

        // TODO: Assert device id is not kAudioObjectUnknown or kAudioObjectSystemObject in C version!
        assert_ne!(stm.output_device.id, kAudioObjectUnknown);
        assert_ne!(stm.output_device.id, kAudioObjectSystemObject);

        stm.output_source_listener = Some(property_listener::new(
            stm.output_device.id, &OUTPUT_DATA_SOURCE_PROPERTY_ADDRESS,
            audiounit_property_listener_callback, stm));
        rv = audiounit_add_listener(stm.output_source_listener.as_ref().unwrap());
        if rv != NO_ERR {
            stm.output_source_listener = None;
            cubeb_log!("AudioObjectAddPropertyListener/output/kAudioDevicePropertyDataSource rv={}, device id={}", rv, stm.output_device.id);
            r = Err(Error::error());
        }
    }

    if !stm.input_unit.is_null() {
        /* This event will notify us when the data source on the input device changes. */

        // TODO: Assert device id is not kAudioObjectUnknown or kAudioObjectSystemObject in C version!
        assert_ne!(stm.input_device.id, kAudioObjectUnknown);
        assert_ne!(stm.input_device.id, kAudioObjectSystemObject);

        stm.input_source_listener = Some(property_listener::new(
            stm.input_device.id, &INPUT_DATA_SOURCE_PROPERTY_ADDRESS,
            audiounit_property_listener_callback, stm));
        rv = audiounit_add_listener(stm.input_source_listener.as_ref().unwrap());
        if rv != NO_ERR {
            stm.input_source_listener = None;
            cubeb_log!("AudioObjectAddPropertyListener/input/kAudioDevicePropertyDataSource rv={}, device id={}", rv, stm.input_device.id);
            r = Err(Error::error());
        }

        /* Event to notify when the input is going away. */
        stm.input_alive_listener = Some(property_listener::new(
            stm.input_device.id, &DEVICE_IS_ALIVE_PROPERTY_ADDRESS,
            audiounit_property_listener_callback, stm));
        rv = audiounit_add_listener(stm.input_alive_listener.as_ref().unwrap());
        if rv != NO_ERR {
            stm.input_alive_listener = None;
            cubeb_log!("AudioObjectAddPropertyListener/input/kAudioDevicePropertyDeviceIsAlive rv={}, device id ={}", rv, stm.input_device.id);
            r = Err(Error::error());
        }
    }

    r
}

fn audiounit_install_system_changed_callback(stm: &mut AudioUnitStream) -> Result<()>
{
    let mut r = NO_ERR;

    if !stm.output_unit.is_null() {
        /* This event will notify us when the default audio device changes,
         * for example when the user plugs in a USB headset and the system chooses it
         * automatically as the default, or when another device is chosen in the
         * dropdown list. */
        stm.default_output_listener = Some(property_listener::new(
            kAudioObjectSystemObject, &DEFAULT_OUTPUT_DEVICE_PROPERTY_ADDRESS,
            audiounit_property_listener_callback, stm));
        r = audiounit_add_listener(stm.default_output_listener.as_ref().unwrap());
        if r != NO_ERR {
            stm.default_output_listener = None;
            cubeb_log!("AudioObjectAddPropertyListener/output/kAudioHardwarePropertyDefaultOutputDevice rv={}", r);
            return Err(Error::error());
        }
    }

    if !stm.input_unit.is_null() {
        /* This event will notify us when the default input device changes. */
        stm.default_input_listener = Some(property_listener::new(
            kAudioObjectSystemObject, &DEFAULT_INPUT_DEVICE_PROPERTY_ADDRESS,
            audiounit_property_listener_callback, stm));
        r = audiounit_add_listener(stm.default_input_listener.as_ref().unwrap());
        if r != NO_ERR {
            stm.default_input_listener = None;
            cubeb_log!("AudioObjectAddPropertyListener/input/kAudioHardwarePropertyDefaultInputDevice rv={}", r);
            return Err(Error::error());
        }
    }

    Ok(())
}

fn audiounit_uninstall_device_changed_callback(stm: &mut AudioUnitStream) -> Result<()>
{
    let mut rv = NO_ERR;
    // Failing to uninstall listeners is not a fatal error.
    let mut r = Ok(());

    if stm.output_source_listener.is_some() {
        rv = audiounit_remove_listener(stm.output_source_listener.as_ref().unwrap());
        if rv != NO_ERR {
            cubeb_log!("AudioObjectRemovePropertyListener/output/kAudioDevicePropertyDataSource rv={}, device id={}", rv, stm.output_device.id);
            r = Err(Error::error());
        }
        stm.output_source_listener = None;
    }

    if stm.input_source_listener.is_some() {
        rv = audiounit_remove_listener(stm.input_source_listener.as_ref().unwrap());
        if rv != NO_ERR {
            cubeb_log!("AudioObjectRemovePropertyListener/input/kAudioDevicePropertyDataSource rv={}, device id={}", rv, stm.input_device.id);
            r = Err(Error::error());
        }
        stm.input_source_listener = None;
    }

    if stm.input_alive_listener.is_some() {
        rv = audiounit_remove_listener(stm.input_alive_listener.as_ref().unwrap());
        if rv != NO_ERR {
            cubeb_log!("AudioObjectRemovePropertyListener/input/kAudioDevicePropertyDeviceIsAlive rv={}, device id={}", rv, stm.input_device.id);
            r = Err(Error::error());
        }
        stm.input_alive_listener = None;
    }

    r
}

fn audiounit_uninstall_system_changed_callback(stm: &mut AudioUnitStream) -> Result<()>
{
    let mut r = NO_ERR;

    if stm.default_output_listener.is_some() {
        r = audiounit_remove_listener(stm.default_output_listener.as_ref().unwrap());
        if r != NO_ERR {
            return Err(Error::error());
        }
        stm.default_output_listener = None;
    }

    if stm.default_input_listener.is_some() {
        r = audiounit_remove_listener(stm.default_input_listener.as_ref().unwrap());
        if r != NO_ERR {
            return Err(Error::error());
        }
        stm.default_input_listener = None;
    }

    Ok(())
}

fn audiounit_get_acceptable_latency_range(latency_range: &mut AudioValueRange) -> Result<()>
{
    let mut size: usize = 0;
    let mut r = NO_ERR;
    let mut output_device_id: AudioDeviceID = kAudioObjectUnknown;
    let output_device_buffer_size_range = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyBufferFrameSizeRange,
        mScope: kAudioDevicePropertyScopeOutput,
        mElement: kAudioObjectPropertyElementMaster,
    };

    output_device_id = audiounit_get_default_device_id(DeviceType::OUTPUT);
    if output_device_id == kAudioObjectUnknown {
        cubeb_log!("Could not get default output device id.");
        return Err(Error::error());
    }

    /* Get the buffer size range this device supports */
    size = mem::size_of_val(latency_range);
    assert_eq!(size, mem::size_of::<AudioValueRange>());

    r = audio_object_get_property_data(output_device_id,
                                       &output_device_buffer_size_range,
                                       &mut size,
                                       latency_range);
    if r != NO_ERR {
        cubeb_log!("AudioObjectGetPropertyData/buffer size range rv={}", r);
        return Err(Error::error());
    }

    Ok(())
}

fn audiounit_get_default_device_id(devtype: DeviceType) -> AudioObjectID
{
    let adr;
    if devtype == DeviceType::OUTPUT {
        adr = &DEFAULT_OUTPUT_DEVICE_PROPERTY_ADDRESS;
    } else if devtype == DeviceType::INPUT {
        adr = &DEFAULT_INPUT_DEVICE_PROPERTY_ADDRESS;
    } else {
        return kAudioObjectUnknown;
    }

    let mut devid: AudioDeviceID = kAudioObjectUnknown;
    let mut size = mem::size_of::<AudioDeviceID>();
    if audio_object_get_property_data(kAudioObjectSystemObject,
                                      adr, &mut size, &mut devid) != NO_ERR {
        return kAudioObjectUnknown;
    }

    return devid;
}

fn audio_stream_desc_init(ss: &mut AudioStreamBasicDescription,
                          stream_params: &StreamParams) -> Result<()>
{
    // TODO:
    //   1. Apply more strict checkings. e.g., min rate should be 44100.
    //   2. C version doesn't check anything. Update it!
    assert!(stream_params.rate() > 0);
    assert!(stream_params.channels() > 0);

    match stream_params.format() {
        SampleFormat::S16LE => {
            ss.mBitsPerChannel = 16;
            ss.mFormatFlags = kAudioFormatFlagIsSignedInteger;
        },
        SampleFormat::S16BE => {
            ss.mBitsPerChannel = 16;
            ss.mFormatFlags = kAudioFormatFlagIsSignedInteger |
                kAudioFormatFlagIsBigEndian;
        },
        SampleFormat::Float32LE => {
            ss.mBitsPerChannel = 32;
            ss.mFormatFlags = kAudioFormatFlagIsFloat;
        },
        SampleFormat::Float32BE => {
            ss.mBitsPerChannel = 32;
            ss.mFormatFlags = kAudioFormatFlagIsFloat |
                kAudioFormatFlagIsBigEndian;
        }
        _ => {
            return Err(Error::invalid_format());
        }
    }

    ss.mFormatID = kAudioFormatLinearPCM;
    ss.mFormatFlags |= kLinearPCMFormatFlagIsPacked;
    ss.mSampleRate = stream_params.rate() as f64;
    ss.mChannelsPerFrame = stream_params.channels();

    ss.mBytesPerFrame = (ss.mBitsPerChannel / 8) * ss.mChannelsPerFrame;
    ss.mFramesPerPacket = 1;
    ss.mBytesPerPacket = ss.mBytesPerFrame * ss.mFramesPerPacket;

    ss.mReserved = 0;

    Ok(())
}

fn audiounit_get_sub_devices(device_id: AudioDeviceID) -> Vec<AudioObjectID>
{
    // FIXIT: Add a check ? We will fail to get data size if `device_id`
    //        is `kAudioObjectUnknown`!
    // assert_ne!(device_id, kAudioObjectUnknown);

    let mut sub_devices = Vec::new();
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioAggregateDevicePropertyActiveSubDeviceList,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster
    };
    let mut size: usize = 0;
    let mut rv = audio_object_get_property_data_size(
        device_id,
        &property_address,
        &mut size
    );

    // NOTE: Hit this if `device_id` is not an aggregate device!
    if rv != NO_ERR {
        sub_devices.push(device_id);
        return sub_devices;
    }

    // TODO: Add a check ? If device_id is a blank aggregate device,
    //       the size is 0! We should just return an empty directly
    //       or get a panic!
    // assert_ne!(size, 0);
    // if size == 0 {
    //     return sub_devices;
    // }

    let count = size / mem::size_of::<AudioObjectID>();
    sub_devices = allocate_array(count);
    // assert_eq!(count, sub_devices.len());
    // assert_eq!(size, sub_devices.len() * mem::size_of::<AudioObjectID>());
    rv = audio_object_get_property_data(
        device_id,
        &property_address,
        &mut size,
        sub_devices.as_mut_ptr()
    );

    if rv != NO_ERR {
        sub_devices.clear();
        sub_devices.push(device_id);
    } else {
        cubeb_log!("Found {} sub-devices", count);
    }
    sub_devices
}

fn audiounit_create_blank_aggregate_device(plugin_id: &mut AudioObjectID, aggregate_device_id: &mut AudioDeviceID) -> Result<()>
{
    let address_plugin_bundle_id = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyPlugInForBundleID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster
    };

    let mut size: usize = 0;
    let mut r = audio_object_get_property_data_size(kAudioObjectSystemObject,
                                                    &address_plugin_bundle_id,
                                                    &mut size);
    if r != NO_ERR {
        // TODO: Replace `AudioHardwareGetPropertyInfo` by `AudioObjectGetPropertyDataSize` ?
        cubeb_log!("AudioHardwareGetPropertyInfo/kAudioHardwarePropertyPlugInForBundleID, rv={}", r);
        return Err(Error::error());
    }
    // TODO: Check if size is larger than 0 ?
    // assert_ne!(size, 0);

    // `rust-bindgen` doesn't support `macro`
    // so we replace `CFSTR` by `cfstringref_from_static_string`.
    let mut in_bundle_ref = cfstringref_from_static_string("com.apple.audio.CoreAudio");
    let mut translation_value = AudioValueTranslation {
        mInputData: &mut in_bundle_ref as *mut CFStringRef as *mut c_void,
        mInputDataSize: mem::size_of_val(&in_bundle_ref) as u32,
        mOutputData: plugin_id as *mut AudioObjectID as *mut c_void,
        mOutputDataSize: mem::size_of_val(plugin_id) as u32,
    };
    // assert_eq!(translation_value.mInputDataSize as usize, mem::size_of::<CFStringRef>());
    // assert_eq!(translation_value.mOutputDataSize as usize, mem::size_of::<AudioObjectID>());

    r = audio_object_get_property_data(kAudioObjectSystemObject,
                                       &address_plugin_bundle_id,
                                       &mut size,
                                       &mut translation_value);
    if r != NO_ERR {
        // TODO: Replace `AudioHardwareGetProperty` by `AudioObjectGetPropertyData` ?
        cubeb_log!("AudioHardwareGetProperty/kAudioHardwarePropertyPlugInForBundleID, rv={}", r);
        return Err(Error::error());
    }
    // TODO: Check if plugin_id is different from the initial value (kAudioObjectUnknown) ?
    // assert_ne!(*plugin_id, 0 /* kAudioObjectUnknown */);

    let create_aggregate_device_address = AudioObjectPropertyAddress {
        mSelector: kAudioPlugInCreateAggregateDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster
    };

    r = audio_object_get_property_data_size(*plugin_id,
                                            &create_aggregate_device_address,
                                            &mut size);
    if r != NO_ERR {
        cubeb_log!("AudioObjectGetPropertyDataSize/kAudioPlugInCreateAggregateDevice, rv={}", r);
        return Err(Error::error());
    }
    // TODO: Check if size is larger than 0 ?
    // assert_ne!(size, 0);

    unsafe {
        let aggregate_device_dict = CFDictionaryCreateMutable(kCFAllocatorDefault, 0,
                                                              &kCFTypeDictionaryKeyCallBacks,
                                                              &kCFTypeDictionaryValueCallBacks);
        let mut timestamp = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        libc::gettimeofday(&mut timestamp, ptr::null_mut());
        let time_id = timestamp.tv_sec as i64 * 1000000 + timestamp.tv_usec as i64;
        // TODO: Check if time_id is larger than 0 ?
        // assert!(time_id > 0);

        let prefix = CString::new(PRIVATE_AGGREGATE_DEVICE_NAME).expect("Fail on creating a cstring as a prefix for an aggregate device");

        // let device_name_string = format!("{}_{}", PRIVATE_AGGREGATE_DEVICE_NAME, time_id);
        // let aggregate_device_name = cfstringref_from_string(&device_name_string);
        let aggregate_device_name = CFStringCreateWithFormat(ptr::null(), ptr::null(), cfstringref_from_static_string("%s_%llx"), prefix.as_ptr(), time_id);
        CFDictionaryAddValue(aggregate_device_dict, cfstringref_from_static_string(AGGREGATE_DEVICE_NAME_KEY) as *const c_void, aggregate_device_name as *const c_void);
        CFRelease(aggregate_device_name as *const c_void);

        // let device_uid_string = format!("org.mozilla.{}_{}", PRIVATE_AGGREGATE_DEVICE_NAME, time_id);
        // let aggregate_device_UID = cfstringref_from_string(&device_uid_string);
        let aggregate_device_UID = CFStringCreateWithFormat(ptr::null(), ptr::null(), cfstringref_from_static_string("org.mozilla.%s_%llx"), prefix.as_ptr(), time_id);
        CFDictionaryAddValue(aggregate_device_dict, cfstringref_from_static_string(AGGREGATE_DEVICE_UID) as *const c_void, aggregate_device_UID as *const c_void);
        CFRelease(aggregate_device_UID as *const c_void);

        let private_value: i32 = 1;
        let aggregate_device_private_key = CFNumberCreate(kCFAllocatorDefault, kCFNumberIntType as i64, &private_value as *const i32 as *const c_void);
        CFDictionaryAddValue(aggregate_device_dict, cfstringref_from_static_string(AGGREGATE_DEVICE_PRIVATE_KEY) as *const c_void, aggregate_device_private_key as *const c_void);
        CFRelease(aggregate_device_private_key as *const c_void);

        let stacked_value: i32 = 0;
        let aggregate_device_stacked_key = CFNumberCreate(kCFAllocatorDefault, kCFNumberIntType as i64, &stacked_value as *const i32 as *const c_void);
        CFDictionaryAddValue(aggregate_device_dict, cfstringref_from_static_string(AGGREGATE_DEVICE_STACKED_KEY) as *const c_void, aggregate_device_stacked_key as *const c_void);
        CFRelease(aggregate_device_stacked_key as *const c_void);

        // assert_eq!(mem::size_of_val(&aggregate_device_dict), mem::size_of::<CFMutableDictionaryRef>());
        // NOTE: This call will fire `audiounit_collection_changed_callback`!
        r = AudioObjectGetPropertyData(*plugin_id,
                                       &create_aggregate_device_address,
                                       mem::size_of_val(&aggregate_device_dict) as u32,
                                       &aggregate_device_dict as *const CFMutableDictionaryRef as *const c_void,
                                       &mut size as *mut usize as *mut u32,
                                       aggregate_device_id as *mut AudioDeviceID as *mut c_void);
        CFRelease(aggregate_device_dict as *const c_void);
        if r != NO_ERR {
            cubeb_log!("AudioObjectGetPropertyData/kAudioPlugInCreateAggregateDevice, rv={}", r);
            return Err(Error::error());
        }
        // TODO: Check if aggregate_device_id is different from the initial value (kAudioObjectUnknown) ?
        // assert_ne!(*aggregate_device_id, 0 /* kAudioObjectUnknown */);
        cubeb_log!("New aggregate device {}", *aggregate_device_id);
    }

    Ok(())
}

fn get_device_name(id: AudioDeviceID) -> CFStringRef
{
    let mut size = mem::size_of::<CFStringRef>();
    let mut UIname: CFStringRef = ptr::null();
    let address_uuid = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceUID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster
    };
    let err = audio_object_get_property_data(id, &address_uuid, &mut size, &mut UIname);
    if err == NO_ERR { UIname } else { ptr::null() }
}

// fn get_device_name(id: AudioDeviceID) -> CString
// {
//     let mut size = mem::size_of::<CFStringRef>();
//     let mut UIname: CFStringRef = ptr::null();
//     let address_uuid = AudioObjectPropertyAddress {
//         mSelector: kAudioDevicePropertyDeviceUID,
//         mScope: kAudioObjectPropertyScopeGlobal,
//         mElement: kAudioObjectPropertyElementMaster
//     };
//     let err = audio_object_get_property_data(id, &address_uuid, &mut size, &mut UIname);
//     if err != NO_ERR {
//         UIname = ptr::null();
//     }
//     audiounit_strref_to_cstr_utf8(UIname)
// }

fn audiounit_set_aggregate_sub_device_list(aggregate_device_id: AudioDeviceID,
                                           input_device_id: AudioDeviceID,
                                           output_device_id: AudioDeviceID) -> Result<()>
{
    // TODO: Check the devices are known ?
    // assert_ne!(aggregate_device_id, kAudioObjectUnknown);
    // assert_ne!(input_device_id, kAudioObjectUnknown);
    // assert_ne!(output_device_id, kAudioObjectUnknown);
    // assert_ne!(input_device_id, output_device_id);

    cubeb_log!("Add devices input {} and output {} into aggregate device {}",
               input_device_id, output_device_id, aggregate_device_id);
    let output_sub_devices = audiounit_get_sub_devices(output_device_id);
    let input_sub_devices = audiounit_get_sub_devices(input_device_id);

    unsafe {
        let aggregate_sub_devices_array = CFArrayCreateMutable(ptr::null(), 0, &kCFTypeArrayCallBacks);
        /* The order of the items in the array is significant and is used to determine the order of the streams
           of the AudioAggregateDevice. */
        // TODO: We will add duplicate devices into the array if there are
        //       common devices in output_sub_devices and input_sub_devices!
        //       (if they are same device or
        //        if either one of them or both of them are aggregate devices)
        //       Should we remove the duplicate devices ?
        for device in output_sub_devices {
            let strref = get_device_name(device);
            if strref.is_null() {
                CFRelease(aggregate_sub_devices_array as *const c_void);
                return Err(Error::error());
            }
            CFArrayAppendValue(aggregate_sub_devices_array, strref as *const c_void);
        }

        for device in input_sub_devices {
            let strref = get_device_name(device);
            if strref.is_null() {
                CFRelease(aggregate_sub_devices_array as *const c_void);
                return Err(Error::error());
            }
            CFArrayAppendValue(aggregate_sub_devices_array, strref as *const c_void);
        }

        let aggregate_sub_device_list = AudioObjectPropertyAddress {
            mSelector: kAudioAggregateDevicePropertyFullSubDeviceList,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMaster
        };

        let size = mem::size_of::<CFMutableArrayRef>();
        let rv = audio_object_set_property_data(aggregate_device_id, &aggregate_sub_device_list, size, &aggregate_sub_devices_array);
        CFRelease(aggregate_sub_devices_array as *const c_void);
        if rv != NO_ERR {
            cubeb_log!("AudioObjectSetPropertyData/kAudioAggregateDevicePropertyFullSubDeviceList, rv={}", rv);
            return Err(Error::error());
        }
    }

    Ok(())
}

fn audiounit_set_master_aggregate_device(aggregate_device_id: AudioDeviceID) -> Result<()>
{
    assert_ne!(aggregate_device_id, kAudioObjectUnknown);
    let master_aggregate_sub_device = AudioObjectPropertyAddress {
        mSelector: kAudioAggregateDevicePropertyMasterSubDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster
    };

    // Master become the 1st output sub device
    let output_device_id = audiounit_get_default_device_id(DeviceType::OUTPUT);
    // TODO: Add a check ?
    // assert_ne!(output_device_id, kAudioObjectUnknown);
    let output_sub_devices = audiounit_get_sub_devices(output_device_id);
    // TODO: Add a check ? or use first instead ?
    // assert!(!output_sub_devices.is_empty());
    // let master_sub_device = get_device_name(output_sub_devices.first().unwrap().clone());
    let master_sub_device = get_device_name(output_sub_devices[0]);
    // TODO: Check if output_sub_devices[0] is in the sub devices list of
    //       the aggregate device ?
    // TODO: Check if this is a NULL CFStringRef ?
    // assert!(!master_sub_device.is_null());

    // NOTE: It's ok if this device is not in the sub devices list,
    //       even if the CFStringRef is a NULL CFStringRef!
    let size = mem::size_of::<CFStringRef>();
    let rv = audio_object_set_property_data(aggregate_device_id,
                                            &master_aggregate_sub_device,
                                            size,
                                            &master_sub_device);
    if rv != NO_ERR {
        cubeb_log!("AudioObjectSetPropertyData/kAudioAggregateDevicePropertyMasterSubDevice, rv={}", rv);
        return Err(Error::error());
    }
    Ok(())
}

fn audiounit_activate_clock_drift_compensation(aggregate_device_id: AudioDeviceID) -> Result<()>
{
    assert_ne!(aggregate_device_id, kAudioObjectUnknown);
    let address_owned = AudioObjectPropertyAddress {
        mSelector: kAudioObjectPropertyOwnedObjects,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster
    };

    let qualifier_data_size = mem::size_of::<AudioObjectID>();
    let class_id: AudioClassID = kAudioSubDeviceClassID;
    let qualifier_data = &class_id;
    let mut size: usize = 0;

    let mut rv = unsafe {
        AudioObjectGetPropertyDataSize(aggregate_device_id,
                                       &address_owned,
                                       qualifier_data_size as u32,
                                       qualifier_data as *const u32 as *const c_void,
                                       &mut size as *mut usize as *mut u32)
    };

    if rv != NO_ERR {
        cubeb_log!("AudioObjectGetPropertyDataSize/kAudioObjectPropertyOwnedObjects, rv={}", rv);
        return Err(Error::error());
    }

    let subdevices_num = size / mem::size_of::<AudioObjectID>();
    let mut sub_devices: Vec<AudioObjectID> = allocate_array(subdevices_num);

    rv = unsafe {
        AudioObjectGetPropertyData(aggregate_device_id,
                                   &address_owned,
                                   qualifier_data_size as u32,
                                   qualifier_data as *const u32 as *const c_void,
                                   &mut size as *mut usize as *mut u32,
                                   sub_devices.as_mut_ptr() as *mut c_void)
    };

    if rv != NO_ERR {
        cubeb_log!("AudioObjectGetPropertyData/kAudioObjectPropertyOwnedObjects, rv={}", rv);
        return Err(Error::error());
    }

    let address_drift = AudioObjectPropertyAddress {
        mSelector: kAudioSubDevicePropertyDriftCompensation,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster
    };

    // Start from the second device since the first is the master clock
    // TODO: Check the list is longer than 1 ?
    // assert!(sub_devices.len() > 1);
    for device in &sub_devices[1..] {
        let drift_compensation_value: u32 = 1;
        rv = audio_object_set_property_data(*device,
                                            &address_drift,
                                            mem::size_of::<u32>(),
                                            &drift_compensation_value);
        if rv != NO_ERR {
            cubeb_log!("AudioObjectSetPropertyData/kAudioSubDevicePropertyDriftCompensation, rv={}", rv);
            return Ok(());
        }
    }

    Ok(())
}

// TODO: If this is only called when airpod is part of the aggregate device,
//       should we add a check for this ?
fn audiounit_workaround_for_airpod(stm: &AudioUnitStream)
{
    let mut input_device_info = ffi::cubeb_device_info::default();
    // TODO: Check input_device.id ? Check if the call is successful ?
    assert_ne!(stm.input_device.id, kAudioObjectUnknown);
    audiounit_create_device_from_hwdev(&mut input_device_info, stm.input_device.id, DeviceType::INPUT);

    let mut output_device_info = ffi::cubeb_device_info::default();
    assert_ne!(stm.output_device.id, kAudioObjectUnknown);
    audiounit_create_device_from_hwdev(&mut output_device_info, stm.output_device.id, DeviceType::OUTPUT);

    // TODO: Check input_device_info.friendly_name and
    //       output_device_info.friendly_name ?
    // NOTE: Retake the leaked friendly_name strings.
    //       It's better to extract the part of getting name of the data source
    //       into a function, so we don't need to call
    //       `audiounit_create_device_from_hwdev` to get this info.
    let input_name_str = unsafe {
        CString::from_raw(input_device_info.friendly_name as *mut c_char)
            .into_string()
            .expect("Fail to convert input name from CString into String")
    };
    input_device_info.friendly_name = ptr::null();
    let output_name_str = unsafe {
        CString::from_raw(output_device_info.friendly_name as *mut c_char)
            .into_string()
            .expect("Fail to convert output name from CString into String")
    };
    output_device_info.friendly_name = ptr::null();

    if input_name_str.contains("AirPods") &&
       output_name_str.contains("AirPods") {
        let mut input_min_rate = 0;
        let mut input_max_rate = 0;
        let mut input_nominal_rate = 0;
        audiounit_get_available_samplerate(stm.input_device.id, kAudioObjectPropertyScopeGlobal,
                                           &mut input_min_rate, &mut input_max_rate, &mut input_nominal_rate);
        cubeb_log!("({:p}) Input device {}, name: {}, min: {}, max: {}, nominal rate: {}", stm, stm.input_device.id
        , input_name_str, input_min_rate, input_max_rate, input_nominal_rate);

        let mut output_min_rate = 0;
        let mut output_max_rate = 0;
        let mut output_nominal_rate = 0;
        audiounit_get_available_samplerate(stm.output_device.id, kAudioObjectPropertyScopeGlobal,
                                           &mut output_min_rate, &mut output_max_rate, &mut output_nominal_rate);
        cubeb_log!("({:p}) Output device {}, name: {}, min: {}, max: {}, nominal rate: {}", stm, stm.output_device.id
        , output_name_str, output_min_rate, output_max_rate, output_nominal_rate);

        let rate = input_nominal_rate as f64;
        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyNominalSampleRate,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMaster
        };

        // TODO: Check the aggregate_device_id ?
        let rv = audio_object_set_property_data(stm.aggregate_device_id,
                                                &addr,
                                                mem::size_of::<f64>(),
                                                &rate);
        if rv != NO_ERR {
            cubeb_log!("Non fatal error, AudioObjectSetPropertyData/kAudioDevicePropertyNominalSampleRate, rv={}", rv);
        }
    }

    // Retrieve the rest lost memory.
    // No need to retrieve the memory of {input,output}_device_info.friendly_name
    // since they are already retrieved/retaken above.
    assert!(input_device_info.friendly_name.is_null());
    audiounit_device_destroy(&mut input_device_info);
    assert!(output_device_info.friendly_name.is_null());
    audiounit_device_destroy(&mut output_device_info);
}

/*
 * Aggregate Device is a virtual audio interface which utilizes inputs and outputs
 * of one or more physical audio interfaces. It is possible to use the clock of
 * one of the devices as a master clock for all the combined devices and enable
 * drift compensation for the devices that are not designated clock master.
 *
 * Creating a new aggregate device programmatically requires [0][1]:
 * 1. Locate the base plug-in ("com.apple.audio.CoreAudio")
 * 2. Create a dictionary that describes the aggregate device
 *    (don't add sub-devices in that step, prone to fail [0])
 * 3. Ask the base plug-in to create the aggregate device (blank)
 * 4. Add the array of sub-devices.
 * 5. Set the master device (1st output device in our case)
 * 6. Enable drift compensation for the non-master devices
 *
 * [0] https://lists.apple.com/archives/coreaudio-api/2006/Apr/msg00092.html
 * [1] https://lists.apple.com/archives/coreaudio-api/2005/Jul/msg00150.html
 * [2] CoreAudio.framework/Headers/AudioHardware.h
 * */
fn audiounit_create_aggregate_device(stm: &mut AudioUnitStream) -> Result<()>
{
    if let Err(r) = audiounit_create_blank_aggregate_device(&mut stm.plugin_id, &mut stm.aggregate_device_id) {
        cubeb_log!("({:p}) Failed to create blank aggregate device", stm);
        return Err(r);
    }

    if let Err(r) = audiounit_set_aggregate_sub_device_list(stm.aggregate_device_id, stm.input_device.id, stm.output_device.id) {
        cubeb_log!("({:p}) Failed to set aggregate sub-device list", stm);
        // TODO: Check if aggregate device is destroyed or not ?
        audiounit_destroy_aggregate_device(stm.plugin_id, &mut stm.aggregate_device_id);
        return Err(r);
    }

    if let Err(r) = audiounit_set_master_aggregate_device(stm.aggregate_device_id) {
        cubeb_log!("({:p}) Failed to set master sub-device for aggregate device", stm);
        // TODO: Check if aggregate device is destroyed or not ?
        audiounit_destroy_aggregate_device(stm.plugin_id, &mut stm.aggregate_device_id);
        return Err(r);
    }

    if let Err(r) = audiounit_activate_clock_drift_compensation(stm.aggregate_device_id) {
        cubeb_log!("({:p}) Failed to activate clock drift compensation for aggregate device", stm);
        // TODO: Check if aggregate device is destroyed or not ?
        audiounit_destroy_aggregate_device(stm.plugin_id, &mut stm.aggregate_device_id);
        return Err(r);
    }

    audiounit_workaround_for_airpod(stm);

    Ok(())
}

fn audiounit_destroy_aggregate_device(plugin_id: AudioObjectID, aggregate_device_id: &mut AudioDeviceID) -> Result<()>
{
    assert_ne!(plugin_id, kAudioObjectUnknown);
    assert_ne!(*aggregate_device_id, kAudioObjectUnknown);

    let destroy_aggregate_device_addr = AudioObjectPropertyAddress {
        mSelector: kAudioPlugInDestroyAggregateDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster
    };

    let mut size: usize = 0;
    let mut rv = audio_object_get_property_data_size(plugin_id,
                                                     &destroy_aggregate_device_addr,
                                                     &mut size);
    if rv != NO_ERR {
        cubeb_log!("AudioObjectGetPropertyDataSize/kAudioPlugInDestroyAggregateDevice, rv={}", rv);
        return Err(Error::error());
    }

    // TODO: Add a check ?
    // assert!(size > 0);

    rv = audio_object_get_property_data(plugin_id,
                                        &destroy_aggregate_device_addr,
                                        &mut size,
                                        aggregate_device_id);
    if rv != NO_ERR {
        cubeb_log!("AudioObjectGetPropertyData/kAudioPlugInDestroyAggregateDevice, rv={}", rv);
        return Err(Error::error());
    }

    cubeb_log!("Destroyed aggregate device {}", *aggregate_device_id);
    // TODO: Use kAudioObjectUnknown instead ?
    *aggregate_device_id = 0;

    Ok(())
}

#[cfg(target_os = "ios")]
fn audiounit_new_unit_instance(unit: &mut AudioUnit, _: &device_info) -> Result<()>
{
    assert!((*unit).is_null());

    let mut desc = AudioComponentDescription::default();
    let mut comp: AudioComponent;
    let mut rv = NO_ERR;

    desc.componentType = kAudioUnitType_Output;
    desc.componentSubType = kAudioUnitSubType_RemoteIO;

    desc.componentManufacturer = kAudioUnitManufacturer_Apple;
    desc.componentFlags = 0;
    desc.componentFlagsMask = 0;
    comp = unsafe { AudioComponentFindNext(ptr::null_mut(), &desc) };
    if comp.is_null() {
        cubeb_log!("Could not find matching audio hardware.");
        return Err(Error::error());
    }

    rv = unsafe { AudioComponentInstanceNew(comp, unit) };
    if rv != NO_ERR {
        cubeb_log!("AudioComponentInstanceNew rv={}", rv);
        return Err(Error::error());
    }
    Ok(())
}

#[cfg(not(target_os = "ios"))]
fn audiounit_new_unit_instance(unit: &mut AudioUnit, device: &device_info) -> Result<()>
{
    assert!((*unit).is_null());

    let mut desc = AudioComponentDescription::default();
    let mut comp: AudioComponent = ptr::null_mut();
    let mut rv = NO_ERR;

    desc.componentType = kAudioUnitType_Output;
    // Use the DefaultOutputUnit for output when no device is specified
    // so we retain automatic output device switching when the default
    // changes.  Once we have complete support for device notifications
    // and switching, we can use the AUHAL for everything.
    if device.flags.contains(device_flags::DEV_SYSTEM_DEFAULT |
                             device_flags::DEV_OUTPUT) {
        desc.componentSubType = kAudioUnitSubType_DefaultOutput;
    } else {
        desc.componentSubType = kAudioUnitSubType_HALOutput;
    }

    desc.componentManufacturer = kAudioUnitManufacturer_Apple;
    desc.componentFlags = 0;
    desc.componentFlagsMask = 0;
    comp = unsafe { AudioComponentFindNext(ptr::null_mut(), &desc) };
    if comp.is_null() {
        cubeb_log!("Could not find matching audio hardware.");
        return Err(Error::error());
    }

    rv = unsafe { AudioComponentInstanceNew(comp, unit as *mut AudioUnit) };
    if rv != NO_ERR {
        cubeb_log!("AudioComponentInstanceNew rv={}", rv);
        return Err(Error::error());
    }
    Ok(())
}

#[derive(PartialEq)]
enum enable_state {
  DISABLE,
  ENABLE,
}

fn audiounit_enable_unit_scope(unit: &AudioUnit, side: io_side, state: enable_state) -> Result<()>
{
    assert!(!(*unit).is_null());

    let mut rv = NO_ERR;
    let enable: u32 = if state == enable_state::DISABLE { 0 } else { 1 };
    rv = audio_unit_set_property(*unit, kAudioOutputUnitProperty_EnableIO,
                                 if side == io_side::INPUT { kAudioUnitScope_Input } else { kAudioUnitScope_Output },
                                 if side == io_side::INPUT { AU_IN_BUS } else { AU_OUT_BUS },
                                 &enable,
                                 mem::size_of::<u32>());
    if rv != NO_ERR {
        cubeb_log!("AudioUnitSetProperty/kAudioOutputUnitProperty_EnableIO rv={}", rv);
        return Err(Error::error());
    }
    Ok(())
}

fn audiounit_create_unit(unit: &mut AudioUnit, device: &device_info) -> Result<()>
{
    assert!((*unit).is_null());

    let mut rv = NO_ERR;
    audiounit_new_unit_instance(unit, device)?;
    assert!(!(*unit).is_null());

    if device.flags.contains(device_flags::DEV_SYSTEM_DEFAULT | device_flags::DEV_OUTPUT) {
        return Ok(());
    }

    if device.flags.contains(device_flags::DEV_INPUT) {
        if let Err(r) = audiounit_enable_unit_scope(unit, io_side::INPUT, enable_state::ENABLE) {
            // TODO: redundant space! Sync with C version.
            cubeb_log!("Failed to enable audiounit input scope ");
            return Err(r);
        }
        if let Err(r) = audiounit_enable_unit_scope(unit, io_side::OUTPUT, enable_state::DISABLE) {
            // TODO: redundant space! Sync with C version.
            cubeb_log!("Failed to disable audiounit output scope ");
            return Err(r);
        }
    } else if device.flags.contains(device_flags::DEV_OUTPUT) {
        if let Err(r) = audiounit_enable_unit_scope(unit, io_side::OUTPUT, enable_state::ENABLE) {
            // TODO: redundant space! Sync with C version.
            cubeb_log!("Failed to enable audiounit output scope ");
            return Err(r);
        }
        if let Err(r) = audiounit_enable_unit_scope(unit, io_side::INPUT, enable_state::DISABLE) {
            // TODO: redundant space! Sync with C version.
            cubeb_log!("Failed to disable audiounit input scope ");
            return Err(r);
        }
    } else {
        assert!(false);
    }

    rv = audio_unit_set_property(*unit,
                                 kAudioOutputUnitProperty_CurrentDevice,
                                 kAudioUnitScope_Global,
                                 0,
                                 &device.id,
                                 mem::size_of::<AudioDeviceID>());
    if rv != NO_ERR {
        cubeb_log!("AudioUnitSetProperty/kAudioOutputUnitProperty_CurrentDevice rv={}", rv);
        return Err(Error::error());
    }

    Ok(())
}

fn audiounit_init_input_linear_buffer(stream: &mut AudioUnitStream, capacity: u32) -> Result<()>
{
    // FIXIT: Make sure `input_desc` is initialized, or the type of the buffer is set to float!
    // assert_ne!(stream.input_desc.mFormatFlags, 0);
    // assert_ne!(stream.input_desc.mChannelsPerFrame, 0);
    // TODO: and latency_frames is larger than zero ?
    // assert_ne!(stream.latency_frames, 0);
    let size = (capacity * stream.latency_frames * stream.input_desc.mChannelsPerFrame) as usize;
    if stream.input_desc.mFormatFlags & kAudioFormatFlagIsSignedInteger != 0 {
        // TODO: Assert input_desc.mFormatFlags doesn't contain kAudioFormatFlagIsFloat ?
        // assert_eq!(stream.input_desc.mFormatFlags & kAudioFormatFlagIsFloat, 0);
        stream.input_linear_buffer = Some(Box::new(AutoArrayImpl::<i16>::new(size)));
    } else {
        // TODO: Assert input_desc.mFormatFlags contains kAudioFormatFlagIsFloat ?
        // assert_ne!(stream.input_desc.mFormatFlags & kAudioFormatFlagIsFloat, 0);
        // TODO: Assert input_desc.mFormatFlags doesn't contain kAudioFormatFlagIsSignedInteger ?
        // assert_eq!(stream.input_desc.mFormatFlags & kAudioFormatFlagIsSignedInteger, 0);
        stream.input_linear_buffer = Some(Box::new(AutoArrayImpl::<f32>::new(size)));
    }
    assert_eq!(stream.input_linear_buffer.as_ref().unwrap().elements(), 0);

    Ok(())
}

// TODO: 1. Change to audiounit_clamp_latency(stm: &mut AudioUnitStream)
//          latency_frames is actually equal to stm.latency_frames.
//       2. Merge the value clamp for boundary.
fn audiounit_clamp_latency(stm: &mut AudioUnitStream, latency_frames: u32) -> u32
{
    // For the 1st stream set anything within safe min-max
    assert!(audiounit_active_streams(stm.context) > 0);
    if audiounit_active_streams(stm.context) == 1 {
        return cmp::max(cmp::min(latency_frames, SAFE_MAX_LATENCY_FRAMES),
                        SAFE_MIN_LATENCY_FRAMES);
    }
    // TODO: Should we check this even for 1 stream case ?
    //       Do we need to set latency if there is no output unit ?
    assert!(!stm.output_unit.is_null());

    // If more than one stream operates in parallel
    // allow only lower values of latency
    let mut r = NO_ERR;
    let mut output_buffer_size: UInt32 = 0;
    let mut size = mem::size_of_val(&output_buffer_size);
    assert_eq!(size, mem::size_of::<UInt32>());
    // TODO: Why we check `output_unit` here? We already have an assertions above!
    if !stm.output_unit.is_null() {
        r = audio_unit_get_property(stm.output_unit,
                                    kAudioDevicePropertyBufferFrameSize,
                                    kAudioUnitScope_Output,
                                    AU_OUT_BUS,
                                    &mut output_buffer_size,
                                    &mut size);
        if r != NO_ERR {
            cubeb_log!("AudioUnitGetProperty/output/kAudioDevicePropertyBufferFrameSize rv={}", r);
            // TODO: Shouldn't it return something in range between
            //       SAFE_MIN_LATENCY_FRAMES and SAFE_MAX_LATENCY_FRAMES ?
            return 0;
        }

        output_buffer_size = cmp::max(cmp::min(output_buffer_size, SAFE_MAX_LATENCY_FRAMES),
                                      SAFE_MIN_LATENCY_FRAMES);
    }

    let mut input_buffer_size: UInt32 = 0;
    if !stm.input_unit.is_null() {
        r = audio_unit_get_property(stm.input_unit,
                                    kAudioDevicePropertyBufferFrameSize,
                                    kAudioUnitScope_Input,
                                    AU_IN_BUS,
                                    &mut input_buffer_size,
                                    &mut size);
        if r != NO_ERR {
            cubeb_log!("AudioUnitGetProperty/input/kAudioDevicePropertyBufferFrameSize rv={}", r);
            // TODO: Shouldn't it return something in range between
            //       SAFE_MIN_LATENCY_FRAMES and SAFE_MAX_LATENCY_FRAMES ?
            return 0;
        }

        input_buffer_size = cmp::max(cmp::min(input_buffer_size, SAFE_MAX_LATENCY_FRAMES),
                                     SAFE_MIN_LATENCY_FRAMES);
    }

    // Every following active streams can only set smaller latency
    let upper_latency_limit = if input_buffer_size != 0 && output_buffer_size != 0 {
        cmp::min(input_buffer_size, output_buffer_size)
    } else if input_buffer_size != 0 {
        input_buffer_size
    } else if output_buffer_size != 0 {
        output_buffer_size
    } else {
        SAFE_MAX_LATENCY_FRAMES
    };

    cmp::max(cmp::min(latency_frames, upper_latency_limit),
             SAFE_MIN_LATENCY_FRAMES)
}

/*
 * Change buffer size is prone to deadlock thus we change it
 * following the steps:
 * - register a listener for the buffer size property
 * - change the property
 * - wait until the listener is executed
 * - property has changed, remove the listener
 * */
extern fn buffer_size_changed_callback(inClientData: *mut c_void,
                                       inUnit: AudioUnit,
                                       inPropertyID: AudioUnitPropertyID,
                                       inScope: AudioUnitScope,
                                       inElement: AudioUnitElement)
{
    let stm = unsafe { &mut *(inClientData as *mut AudioUnitStream) };

    let au = inUnit;
    let mut au_scope = kAudioUnitScope_Input;
    let au_element = inElement;
    let mut au_type = "output";

    if AU_IN_BUS == inElement {
        au_scope = kAudioUnitScope_Output;
        au_type = "input";
    }

    match inPropertyID {
        // Using coreaudio_sys as prefix so kAudioDevicePropertyBufferFrameSize
        // won't be treated as a new variable introduced in the match arm.
        coreaudio_sys::kAudioDevicePropertyBufferFrameSize => {
            if inScope != au_scope { // filter out the callback for global scope
                return;
            }
            let mut new_buffer_size: u32 = 0;
            let mut outSize = mem::size_of::<u32>();
            let r = audio_unit_get_property(au,
                                            kAudioDevicePropertyBufferFrameSize,
                                            au_scope,
                                            au_element,
                                            &mut new_buffer_size,
                                            &mut outSize);
            if r != NO_ERR {
                cubeb_log!("({:p}) Event: kAudioDevicePropertyBufferFrameSize: Cannot get current buffer size", stm);
            } else {
                cubeb_log!("({:p}) Event: kAudioDevicePropertyBufferFrameSize: New {} buffer size = {} for scope {}", stm,
                           au_type, new_buffer_size, inScope);
            }
            *stm.buffer_size_change_state.get_mut() = true;
        }
        _ => {}
    }
}

fn audiounit_set_buffer_size(stm: &mut AudioUnitStream, new_size_frames: u32, side: io_side) -> Result<()>
{
    // TODO: Check `new_size_frames` is not zero (larger than zero) ?
    // Surprisingly, it's ok to set `new_size_frames` to zero without getting
    // any error. However, the `buffer frames size` won't become 0 even it's
    // ok to set it to 0. Maybe we should fix it!

    let mut au = stm.output_unit;
    let mut au_scope = kAudioUnitScope_Input;
    let mut au_element = AU_OUT_BUS;

    if side == io_side::INPUT {
        au = stm.input_unit;
        au_scope = kAudioUnitScope_Output;
        au_element = AU_IN_BUS;
    }
    // TODO: Check au is not null ?

    let mut buffer_frames: u32 = 0;
    let mut size = mem::size_of_val(&buffer_frames);
    let mut r = audio_unit_get_property(au,
                                        kAudioDevicePropertyBufferFrameSize,
                                        au_scope,
                                        au_element,
                                        &mut buffer_frames,
                                        &mut size);
    if r != NO_ERR {
        cubeb_log!("AudioUnitGetProperty/{}/kAudioDevicePropertyBufferFrameSize rv={}", to_string(&side), r);
        return Err(Error::error());
    }

    // TODO: Check buffer_frames is not zero (larger than zero) ?
    // TODO: Check new_size_frames is not zero (larger than zero) ?

    if new_size_frames == buffer_frames {
        cubeb_log!("({:p}) No need to update {} buffer size already {} frames", stm, to_string(&side), buffer_frames);
        return Ok(());
    }

    r = audio_unit_add_property_listener(au,
                                         kAudioDevicePropertyBufferFrameSize,
                                         buffer_size_changed_callback,
                                         stm as *mut AudioUnitStream as *mut c_void);
    if r != NO_ERR {
        cubeb_log!("AudioUnitAddPropertyListener/{}/kAudioDevicePropertyBufferFrameSize rv={}", to_string(&side), r);
        return Err(Error::error());
    }

    *stm.buffer_size_change_state.get_mut() = false;

    r = audio_unit_set_property(au,
                                kAudioDevicePropertyBufferFrameSize,
                                au_scope,
                                au_element,
                                &new_size_frames,
                                mem::size_of_val(&new_size_frames));
    if r != NO_ERR {
        cubeb_log!("AudioUnitSetProperty/{}/kAudioDevicePropertyBufferFrameSize rv={}", to_string(&side), r);

        r = audio_unit_remove_property_listener_with_user_data(au,
                                                               kAudioDevicePropertyBufferFrameSize,
                                                               buffer_size_changed_callback,
                                                               stm as *mut AudioUnitStream as *mut c_void);
        if r != NO_ERR {
            cubeb_log!("AudioUnitAddPropertyListener/{}/kAudioDevicePropertyBufferFrameSize rv={}", to_string(&side), r);
        }

        return Err(Error::error());
    }

    let mut count: u32 = 0;
    while !*stm.buffer_size_change_state.get_mut() && count < 30 {
        count += 1;
        // TODO: Log time ...
        cubeb_log!("({:p}) audiounit_set_buffer_size : wait count = {}", stm, count);
    }

    r = audio_unit_remove_property_listener_with_user_data(au,
                                                           kAudioDevicePropertyBufferFrameSize,
                                                           buffer_size_changed_callback,
                                                           stm as *mut AudioUnitStream as *mut c_void);
    if r != NO_ERR {
        cubeb_log!("AudioUnitAddPropertyListener/{}/kAudioDevicePropertyBufferFrameSize rv={}", to_string(&side), r);
        return Err(Error::error());
    }

    if !*stm.buffer_size_change_state.get_mut() && count >= 30 {
        cubeb_log!("({:p}) Error, did not get buffer size change callback ...", stm);
        return Err(Error::error());
    }

    cubeb_log!("({:p}) {} buffer size changed to {} frames.", stm, to_string(&side), new_size_frames);
    Ok(())
}

fn audiounit_configure_input(stm: &mut AudioUnitStream) -> Result<()>
{
    assert!(!stm.input_unit.is_null());

    let mut r = NO_ERR;
    let mut size: usize = 0;
    let mut aurcbs_in = AURenderCallbackStruct::default();

    cubeb_log!("({:p}) Opening input side: rate {}, channels {}, format {:?}, latency in frames {}.",
        stm, stm.input_stream_params.rate(), stm.input_stream_params.channels(),
        stm.input_stream_params.format(), stm.latency_frames);

    /* Get input device sample rate. */
    let mut input_hw_desc = AudioStreamBasicDescription::default();
    size = mem::size_of::<AudioStreamBasicDescription>();
    r = audio_unit_get_property(stm.input_unit,
                                kAudioUnitProperty_StreamFormat,
                                kAudioUnitScope_Input,
                                AU_IN_BUS,
                                &mut input_hw_desc,
                                &mut size);
    if r != NO_ERR {
        cubeb_log!("AudioUnitGetProperty/input/kAudioUnitProperty_StreamFormat rv={}", r);
        return Err(Error::error());
    }
    stm.input_hw_rate = input_hw_desc.mSampleRate;
    cubeb_log!("({:p}) Input device sampling rate: {}", stm, stm.input_hw_rate);

    /* Set format description according to the input params. */
    if let Err(r) = audio_stream_desc_init(&mut stm.input_desc, &stm.input_stream_params) {
        cubeb_log!("({:p}) Setting format description for input failed.", stm);
        return Err(r);
    }

    // Use latency to set buffer size
    // TODO: Make sure stm.latency_frames is larger than 0 ?
    // assert_ne!(stm.latency_frames, 0);
    // Surprisingly, it's ok to set buffer frame size to zero without getting
    // any error. However, the buffer frame size won't become 0 even it's ok to
    // set that. Maybe we should fix it!
    // Use a temporary variable `latency_frames` to avoid borrowing issue.
    let latency_frames = stm.latency_frames;
    if let Err(r) = audiounit_set_buffer_size(stm, latency_frames, io_side::INPUT) {
        cubeb_log!("({:p}) Error in change input buffer size.", stm);
        return Err(r);
    }

    let mut src_desc = stm.input_desc;
    /* Input AudioUnit must be configured with device's sample rate.
       we will resample inside input callback. */
    src_desc.mSampleRate = stm.input_hw_rate;

    r = audio_unit_set_property(stm.input_unit,
                                kAudioUnitProperty_StreamFormat,
                                kAudioUnitScope_Output,
                                AU_IN_BUS,
                                &src_desc,
                                mem::size_of::<AudioStreamBasicDescription>());
    if r != NO_ERR {
        cubeb_log!("AudioUnitSetProperty/input/kAudioUnitProperty_StreamFormat rv={}", r);
        return Err(Error::error());
    }

    // TODO: Surprisingly, it's ok to set frames per slice to zero without
    // getting any error. However, the frames per slice won't become 0 even
    // it's ok to set that. Maybe we should fix it!
    /* Frames per buffer in the input callback. */
    r = audio_unit_set_property(stm.input_unit,
                                kAudioUnitProperty_MaximumFramesPerSlice,
                                kAudioUnitScope_Global,
                                AU_IN_BUS,
                                &stm.latency_frames,
                                mem::size_of::<u32>());
    if r != NO_ERR {
        cubeb_log!("AudioUnitSetProperty/input/kAudioUnitProperty_MaximumFramesPerSlice rv={}", r);
        return Err(Error::error());
    }

    // Input only capacity
    let mut array_capacity = 1;
    if has_output(stm) {
        // Full-duplex increase capacity
        array_capacity = 8;
    }
    if audiounit_init_input_linear_buffer(stm, array_capacity).is_err() {
        return Err(Error::error());
    }

    aurcbs_in.inputProc = Some(audiounit_input_callback);
    aurcbs_in.inputProcRefCon = stm as *mut AudioUnitStream as *mut c_void;

    r = audio_unit_set_property(stm.input_unit,
                                kAudioOutputUnitProperty_SetInputCallback,
                                kAudioUnitScope_Global,
                                AU_OUT_BUS,
                                &aurcbs_in,
                                mem::size_of_val(&aurcbs_in));
    if r != NO_ERR {
        cubeb_log!("AudioUnitSetProperty/input/kAudioOutputUnitProperty_SetInputCallback rv={}", r);
        return Err(Error::error());
    }

    *stm.frames_read.get_mut() = 0;

    cubeb_log!("({:p}) Input audiounit init successfully.", stm);

    Ok(())
}

fn audiounit_configure_output(stm: &mut AudioUnitStream) -> Result<()>
{
    assert!(!stm.output_unit.is_null());

    let mut r = NO_ERR;
    let mut aurcbs_out = AURenderCallbackStruct::default();
    let mut size: usize = 0;

    cubeb_log!("({:p}) Opening output side: rate {}, channels {}, format {:?}, latency in frames {}.",
               stm, stm.output_stream_params.rate(), stm.output_stream_params.channels(),
               stm.output_stream_params.format(), stm.latency_frames);

    if let Err(r) = audio_stream_desc_init(&mut stm.output_desc, &stm.output_stream_params) {
        cubeb_log!("({:p}) Could not initialize the audio stream description.", stm);
        return Err(r);
    }

    /* Get output device sample rate. */
    let mut output_hw_desc = AudioStreamBasicDescription::default();
    size = mem::size_of::<AudioStreamBasicDescription>();
    // C version uses `memset` to set output_hw_desc to an zero value, but
    // AudioStreamBasicDescription::default() return an zero value already
    // so we don't need to do anything here.
    r = audio_unit_get_property(stm.output_unit,
                                kAudioUnitProperty_StreamFormat,
                                kAudioUnitScope_Output,
                                AU_OUT_BUS,
                                &mut output_hw_desc,
                                &mut size);
    if r != NO_ERR {
        cubeb_log!("AudioUnitGetProperty/output/kAudioUnitProperty_StreamFormat rv={}", r);
        return Err(Error::error());
    }
    stm.output_hw_rate = output_hw_desc.mSampleRate;
    cubeb_log!("{:p} Output device sampling rate: {}", stm, output_hw_desc.mSampleRate);

    // TODO: Set channels, layout, ...
    r = audio_unit_set_property(stm.output_unit,
                                kAudioUnitProperty_StreamFormat,
                                kAudioUnitScope_Input,
                                AU_OUT_BUS,
                                &stm.output_desc,
                                mem::size_of::<AudioStreamBasicDescription>());
    if r != NO_ERR {
        cubeb_log!("AudioUnitSetProperty/output/kAudioUnitProperty_StreamFormat rv={}", r);
        return Err(Error::error());
    }

    // Use latency to set buffer size
    // TODO: Make sure stm.latency_frames is larger than 0 ?
    // assert_ne!(stm.latency_frames, 0);
    // Surprisingly, it's ok to set buffer frame size to zero without getting
    // any error. However, the buffer frame size won't become 0 even it's ok to
    // set that. Maybe we should fix it!
    // Use a temporary variable `latency_frames` to avoid borrowing issue.
    let latency_frames = stm.latency_frames;
    if let Err(r) = audiounit_set_buffer_size(stm, latency_frames, io_side::OUTPUT) {
        cubeb_log!("({:p}) Error in change output buffer size.", stm);
        return Err(r);
    }

    /* Frames per buffer in the input callback. */
    r = audio_unit_set_property(stm.output_unit,
                                kAudioUnitProperty_MaximumFramesPerSlice,
                                kAudioUnitScope_Global,
                                AU_OUT_BUS,
                                &stm.latency_frames,
                                mem::size_of::<u32>());
    if r != NO_ERR {
        cubeb_log!("AudioUnitSetProperty/output/kAudioUnitProperty_MaximumFramesPerSlice rv={}", r);
        return Err(Error::error());
    }

    // TODO: Set output callback ...
    aurcbs_out.inputProc = Some(audiounit_output_callback);
    aurcbs_out.inputProcRefCon = stm as *mut AudioUnitStream as *mut c_void;
    r = audio_unit_set_property(stm.output_unit,
                                kAudioUnitProperty_SetRenderCallback,
                                kAudioUnitScope_Global,
                                AU_OUT_BUS,
                                &aurcbs_out,
                                mem::size_of_val(&aurcbs_out));
    if r != NO_ERR {
        cubeb_log!("AudioUnitSetProperty/output/kAudioUnitProperty_SetRenderCallback rv={}", r);
        return Err(Error::error());
    }

    // TODO: Set frames_written to 0 ...

    cubeb_log!("({:p}) Output audiounit init successfully.", stm);
    Ok(())
}

fn audiounit_setup_stream(stm: &mut AudioUnitStream) -> Result<()>
{
    // TODO: Add stm.context.mutex.assert_current_thread_owns() ?
    //       audiounit_active_streams will require to own the mutex in
    //       stm.context.
    stm.mutex.assert_current_thread_owns();

    if stm.input_stream_params.prefs().contains(StreamPrefs::LOOPBACK) ||
       stm.output_stream_params.prefs().contains(StreamPrefs::LOOPBACK) {
        cubeb_log!("({:p}) Loopback not supported for audiounit.", stm);
        return Err(Error::not_supported());
    }

    let mut in_dev_info = stm.input_device.clone();
    let mut out_dev_info = stm.output_device.clone();

    if has_input(stm) && has_output(stm) &&
       stm.input_device.id != stm.output_device.id {
        if let Err(r) = audiounit_create_aggregate_device(stm) {
            // TODO: Use kAudioObjectUnknown instead ?
            stm.aggregate_device_id = 0;
            cubeb_log!("({:p}) Create aggregate devices failed.", stm);
            // !!!NOTE: It is not necessary to return here. If it does not
            // return it will fallback to the old implementation. The intention
            // is to investigate how often it fails. I plan to remove
            // it after a couple of weeks.
        } else {
            in_dev_info.id = stm.aggregate_device_id;
            out_dev_info.id = stm.aggregate_device_id;
            in_dev_info.flags = device_flags::DEV_INPUT;
            out_dev_info.flags = device_flags::DEV_OUTPUT;
        }
    }

    if has_input(stm) {
        if let Err(r) = audiounit_create_unit(&mut stm.input_unit, &in_dev_info) {
            cubeb_log!("({:p}) AudioUnit creation for input failed.", stm);
            return Err(r);
        }
    }

    if has_output(stm) {
        if let Err(r) = audiounit_create_unit(&mut stm.output_unit, &out_dev_info) {
            cubeb_log!("({:p}) AudioUnit creation for output failed.", stm);
            return Err(r);
        }
    }

    /* Latency cannot change if another stream is operating in parallel. In this case
     * latecy is set to the other stream value. */
    if audiounit_active_streams(stm.context) > 1 {
        cubeb_log!("({:p}) More than one active stream, use global latency.", stm);
        stm.latency_frames = stm.context.global_latency_frames;
    } else {
        /* Silently clamp the latency down to the platform default, because we
         * synthetize the clock from the callbacks, and we want the clock to update
         * often. */
        // Create a `latency_frames` here to avoid the borrowing issue.
        let latency_frames = stm.latency_frames;
        // TODO: Change `audiounit_clamp_latency` to audiounit_clamp_latency(stm)!
        stm.latency_frames = audiounit_clamp_latency(stm, latency_frames);
        assert!(stm.latency_frames > 0); // Ungly error check
        audiounit_set_global_latency(stm.context, stm.latency_frames);
    }

    /* Configure I/O stream */
    if has_input(stm) {
        if let Err(r) = audiounit_configure_input(stm) {
            cubeb_log!("({:p}) Configure audiounit input failed.", stm);
            return Err(r);
        }
    }

    if has_output(stm) {
        if let Err(r) = audiounit_configure_output(stm) {
            cubeb_log!("({:p}) Configure audiounit output failed.", stm);
            return Err(r);
        }
    }

    /* We use a resampler because input AudioUnit operates
     * reliable only in the capture device sample rate.
     * Resampler will convert it to the user sample rate
     * and deliver it to the callback. */
    let target_sample_rate = if has_input(stm) {
        stm.input_stream_params.rate()
    } else {
        assert!(has_output(stm));
        stm.output_stream_params.rate()
    };

    let mut input_unconverted_params: ffi::cubeb_stream_params = unsafe { ::std::mem::zeroed() };
    if has_input(stm) {
        input_unconverted_params = unsafe { (*(stm.input_stream_params.as_ptr())).clone() };
        input_unconverted_params.rate = stm.input_hw_rate as u32;
    }

    let stm_ptr = stm as *mut AudioUnitStream as *mut ffi::cubeb_stream;
    let stm_has_input = has_input(stm);
    let stm_has_output = has_output(stm);
    stm.resampler.reset(unsafe {
        ffi::cubeb_resampler_create(
            stm_ptr,
            if stm_has_input { &mut input_unconverted_params } else { ptr::null_mut() },
            if stm_has_output { stm.output_stream_params.as_ptr() } else { ptr::null_mut() },
            target_sample_rate,
            stm.data_callback,
            stm.user_ptr,
            ffi::CUBEB_RESAMPLER_QUALITY_DESKTOP
        )
    });

    if stm.resampler.as_mut_ptr().is_null() {
        cubeb_log!("({:p}) Could not create resampler.", stm);
        return Err(Error::error());
    }

    if !stm.input_unit.is_null() {
        let r = audio_unit_initialize(stm.input_unit);
        if r != NO_ERR {
            cubeb_log!("AudioUnitInitialize/input rv={}", r);
            return Err(Error::error());
        }
    }

    if !stm.output_unit.is_null() {
        let r = audio_unit_initialize(stm.output_unit);
        if r != NO_ERR {
            cubeb_log!("AudioUnitInitialize/output rv={}", r);
            return Err(Error::error());
        }

        *stm.current_latency_frames.get_mut() = audiounit_get_device_presentation_latency(stm.output_device.id, kAudioDevicePropertyScopeOutput);

        let mut unit_s: f64 = 0.0;
        let mut size = mem::size_of_val(&unit_s);
        if audio_unit_get_property(stm.output_unit, kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0, &mut unit_s, &mut size) == NO_ERR {
            *stm.current_latency_frames.get_mut() += (unit_s * stm.output_desc.mSampleRate) as u32
        }
    }

    if !stm.input_unit.is_null() && !stm.output_unit.is_null() {
        // According to the I/O hardware rate it is expected a specific pattern of callbacks
        // for example is input is 44100 and output is 48000 we expected no more than 2
        // out callback in a row.
        // TODO: Make sure `input_hw_rate` is larger than 0 ?
        stm.expected_output_callbacks_in_a_row = (stm.output_hw_rate / stm.input_hw_rate).ceil() as i32
    }

    if let Err(_) = audiounit_install_device_changed_callback(stm) {
        cubeb_log!("({:p}) Could not install all device change callback.", stm);
    }

    Ok(())
}

fn audiounit_close_stream(stm: &mut AudioUnitStream)
{
    stm.mutex.assert_current_thread_owns();

    if !stm.input_unit.is_null() {
        audio_unit_uninitialize(stm.input_unit);
        dispose_audio_unit(stm.input_unit);
        stm.input_unit = ptr::null_mut();
    }

    if !stm.output_unit.is_null() {
        audio_unit_uninitialize(stm.output_unit);
        dispose_audio_unit(stm.output_unit);
        stm.output_unit = ptr::null_mut();
    }

    stm.resampler.reset(ptr::null_mut());
    // TODO: Reset mixer ...

    if stm.aggregate_device_id != kAudioObjectUnknown {
        // TODO: Check if aggregate device is destroyed or not ?
        audiounit_destroy_aggregate_device(stm.plugin_id, &mut stm.aggregate_device_id);
        stm.aggregate_device_id = kAudioObjectUnknown;
    }
}

fn audiounit_stream_destroy_internal(stm: &mut AudioUnitStream)
{
    stm.context.mutex.assert_current_thread_owns();

    if let Err(_) = audiounit_uninstall_system_changed_callback(stm) {
        cubeb_log!("({:p}) Could not uninstall the device changed callback", stm);
    }

    if let Err(_) = audiounit_uninstall_device_changed_callback(stm) {
        cubeb_log!("({:p}) Could not uninstall all device change listeners", stm);
    }

    // The scope of `_lock` is a critical section.
    let mutex_ptr = &mut stm.mutex as *mut OwnedCriticalSection;
    let _lock = AutoLock::new(unsafe { &mut (*mutex_ptr) });
    audiounit_close_stream(stm);
    assert!(audiounit_active_streams(&mut stm.context) >= 1);
    audiounit_decrement_active_streams(&mut stm.context);
}

fn audiounit_stream_destroy(stm: &mut AudioUnitStream)
{
    if !stm.shutdown.load(Ordering::SeqCst) {
        // Since we cannot call `AutoLock::new(&mut stm.context.mutex)` and
        // `audiounit_stream_destroy_internal(stm)` at the same time,
        // We take the pointer to `stm.context.mutex` first and then dereference
        // it to the mutex to avoid this problem for now.
        let mutex_ptr = &mut stm.context.mutex as *mut OwnedCriticalSection;
        // The scope of `_context_lock` is a critical section.
        let _context_lock = AutoLock::new(unsafe { &mut (*mutex_ptr) });
        audiounit_stream_stop_internal(stm);
        *stm.shutdown.get_mut() = true;
    }

    *stm.destroy_pending.get_mut() = true;
    // Rust compilter doesn't allow a pointer to be passed across threads.
    // A hacky way to do that is to cast the pointer into a value, then
    // the value, which is actually an address, can be copied into threads.
    let stm_ptr = stm as *mut AudioUnitStream as usize;
    // Execute close in serial queue to avoid collision
    // with reinit when un/plug devices
    sync_dispatch(stm.context.serial_queue, move || {
        let stm = unsafe { &mut (*(stm_ptr as *mut AudioUnitStream)) };
        // Use `mutex_ptr` to avoid the same borrowing issue as above.
        let mutex_ptr = &mut stm.context.mutex as *mut OwnedCriticalSection;
        // The scope of `_context_lock` is a critical section.
        let _context_lock = AutoLock::new(unsafe { &mut (*mutex_ptr) });
        audiounit_stream_destroy_internal(stm);
    });

    cubeb_log!("Cubeb stream ({:p}) destroyed successful.", stm);
}

fn audiounit_stream_start_internal(stm: &AudioUnitStream)
{
    if !stm.input_unit.is_null() {
        assert_eq!(audio_output_unit_start(stm.input_unit), NO_ERR);
    }
    if !stm.output_unit.is_null() {
        assert_eq!(audio_output_unit_start(stm.output_unit), NO_ERR);
    }
}

fn audiounit_stream_stop_internal(stm: &AudioUnitStream)
{
    if !stm.input_unit.is_null() {
        assert_eq!(audio_output_unit_stop(stm.input_unit), NO_ERR);
    }
    if !stm.output_unit.is_null() {
        assert_eq!(audio_output_unit_stop(stm.output_unit), NO_ERR);
    }
}

fn audiounit_stream_get_volume(stm: &AudioUnitStream, volume: &mut f32) -> Result<()>
{
    assert!(!stm.output_unit.is_null());
    let r = audio_unit_get_parameter(stm.output_unit,
                                     kHALOutputParam_Volume,
                                     kAudioUnitScope_Global,
                                     0, volume);
    if r != NO_ERR {
        cubeb_log!("AudioUnitGetParameter/kHALOutputParam_Volume rv={}", r);
        return Err(Error::error());
    }
    Ok(())
}

fn convert_uint32_into_string(data: u32) -> CString
{
    // Simply create an empty string if no data.
    let empty = CString::default();
    if data == 0 {
        return empty;
    }

    // Reverse 0xWXYZ into 0xZYXW.
    let mut buffer = vec![b'\x00'; 4]; // 4 bytes for uint32.
    buffer[0] = (data >> 24) as u8;
    buffer[1] = (data >> 16) as u8;
    buffer[2] = (data >> 8) as u8;
    buffer[3] = (data) as u8;

    // CString::new() will consume the input bytes vec and add a '\0' at the
    // end of the bytes. The input bytes vec must not contain any 0 bytes in
    // it, in case causing memory leaks when we leak its memory to the
    // external code and then retake the ownership of its memory.
    // https://doc.rust-lang.org/std/ffi/struct.CString.html#method.new
    CString::new(buffer).unwrap_or(empty)
}

fn audiounit_get_default_device_datasource(devtype: DeviceType,
                                           data: &mut u32) -> Result<()>
{
    let id = audiounit_get_default_device_id(devtype);
    if id == kAudioObjectUnknown {
        return Err(Error::error());
    }

    let mut size = mem::size_of_val(data);
    assert_eq!(size, mem::size_of::<u32>());
    // TODO: devtype includes input, output, in-out, and unknown. This is a
    //       bad style to check type, although this function will early return
    //       for in-out and unknown type since audiounit_get_default_device_id
    //       will gives a kAudioObjectUnknown for unknown type.
    /* This fails with some USB headsets (e.g., Plantronic .Audio 628). */
    let r = audio_object_get_property_data(id, if devtype == DeviceType::INPUT {
                                                   &INPUT_DATA_SOURCE_PROPERTY_ADDRESS
                                               } else {
                                                   &OUTPUT_DATA_SOURCE_PROPERTY_ADDRESS
                                               }, &mut size, data);
    if r != NO_ERR {
        *data = 0;
    }

    Ok(())
}

// TODO: This actually is the name converted from the bytes of the data source
//       (kAudioDevicePropertyDataSource), rather than the name of the audio
//       device(kAudioObjectPropertyName). The naming here is vague.
fn audiounit_get_default_device_name(stm: &AudioUnitStream,
                                     device: &mut ffi::cubeb_device,
                                     devtype: DeviceType) -> Result<()>
{
    let mut data: u32 = 0;
    audiounit_get_default_device_datasource(devtype, &mut data)?;

    // TODO: devtype includes input, output, in-out, and unknown. This is a
    //       bad style to check type, although this function will early return
    //       for in-out and unknown type since
    //       audiounit_get_default_device_datasource will throw an error for
    //       in-out and unknown type.
    let name = if devtype == DeviceType::INPUT {
        &mut device.input_name
    } else {
        &mut device.output_name
    };
    // Leak the memory to the external code.
    *name = convert_uint32_into_string(data).into_raw();
    if name.is_null() {
        // TODO: Bad style to use scope as the above.
        cubeb_log!("({:p}) name of {} device is empty!", stm,
                   if devtype == DeviceType::INPUT { "input" } else { "output" } );
    }
    Ok(())
}

fn audiounit_strref_to_cstr_utf8(strref: CFStringRef) -> CString
{
    let empty = CString::default();
    if strref.is_null() {
        return empty;
    }

    let len = unsafe {
        CFStringGetLength(strref)
    };
    // Add 1 to size to allow for '\0' termination character.
    let size = unsafe {
        CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1
    };
    let mut buffer = vec![b'\x00'; size as usize];

    let success = unsafe {
        CFStringGetCString(
            strref,
            buffer.as_mut_ptr() as *mut c_char,
            size,
            kCFStringEncodingUTF8
        ) != 0
    };
    if !success {
        buffer.clear();
        return empty;
    }

    // CString::new() will consume the input bytes vec and add a '\0' at the
    // end of the bytes. We need to remove the '\0' from the bytes data
    // returned from CFStringGetCString by ourselves to avoid memory leaks.
    // https://doc.rust-lang.org/std/ffi/struct.CString.html#method.new
    // The size returned from CFStringGetMaximumSizeForEncoding is always
    // greater than or equal to the string length, where the string length
    // is the number of characters from the beginning to nul-terminator('\0'),
    // so we should shrink the string vector to fit that size.
    let str_len = unsafe {
        libc::strlen(buffer.as_ptr() as *mut c_char)
    };
    buffer.truncate(str_len); // Drop the elements from '\0'(including '\0').

    CString::new(buffer).unwrap_or(empty)
}

fn audiounit_get_channel_count(devid: AudioObjectID, scope: AudioObjectPropertyScope) -> u32
{
    let mut count: u32 = 0;
    let mut size: usize = 0;

    let adr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreamConfiguration,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMaster
    };

    if audio_object_get_property_data_size(devid, &adr, &mut size) == NO_ERR && size > 0 {
        let mut data: Vec<u8> = allocate_array_by_size(size);
        let ptr = data.as_mut_ptr() as *mut AudioBufferList;
        if audio_object_get_property_data(devid, &adr, &mut size, ptr) == NO_ERR {
            // Cannot dereference *ptr to a AudioBufferList directly
            // since it's a variable-size struct: https://bit.ly/2CYFhJ0
            // `let list: = unsafe { *ptr }` will copy the `*ptr` whose type
            // is AudioBufferList to a list. However, it contains only one
            // `UInt32` and only one `AudioBuffer`, while the memory pointed
            // by `ptr` may have one `UInt32` and lots of `AudioBuffer`s.
            // See reference:
            // https://bit.ly/2O2MJE4
            let list: &AudioBufferList = unsafe { &(*ptr) };
            let ptr = list.mBuffers.as_ptr() as *const AudioBuffer;
            let len = list.mNumberBuffers as usize;
            if len == 0 {
                return 0;
            }
            let buffers = unsafe {
                slice::from_raw_parts(ptr, len)
            };
            for buffer in buffers {
                count += buffer.mNumberChannels;
            }
        }
    }
    count
}

// TODO: It seems that it works no matter what scope is(see test.rs). Is it ok?
fn audiounit_get_available_samplerate(devid: AudioObjectID, scope: AudioObjectPropertyScope,
                                      min: &mut u32, max: &mut u32, def: &mut u32)
{
    let mut adr = AudioObjectPropertyAddress {
        mSelector: 0,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMaster
    };

    adr.mSelector = kAudioDevicePropertyNominalSampleRate;
    if audio_object_has_property(devid, &adr) {
        let mut size = mem::size_of::<f64>();
        let mut fvalue: f64 = 0.0;
        if audio_object_get_property_data(devid, &adr, &mut size, &mut fvalue) == NO_ERR {
            *def = fvalue as u32;
        }
    }

    adr.mSelector = kAudioDevicePropertyAvailableNominalSampleRates;
    let mut size = 0;
    let mut range = AudioValueRange::default();
    if audio_object_has_property(devid, &adr) &&
       audio_object_get_property_data_size(devid, &adr, &mut size) == NO_ERR {
        let mut ranges: Vec<AudioValueRange> = allocate_array_by_size(size);
        range.mMinimum = 9999999999.0; // TODO: why not f64::MAX?
        range.mMaximum = 0.0; // TODO: why not f64::MIN?
        if audio_object_get_property_data(devid, &adr, &mut size, ranges.as_mut_ptr()) == NO_ERR {
            for rng in &ranges {
                if rng.mMaximum > range.mMaximum {
                    range.mMaximum = rng.mMaximum;
                }
                if rng.mMinimum < range.mMinimum {
                    range.mMinimum = rng.mMinimum;
                }
            }
        }
        *max = range.mMaximum as u32;
        *min = range.mMinimum as u32;
    } else {
        *max = 0;
        *min = 0;
    }
}

fn audiounit_get_device_presentation_latency(devid: AudioObjectID, scope: AudioObjectPropertyScope) -> u32
{
    let mut adr = AudioObjectPropertyAddress {
        mSelector: 0,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMaster
    };
    let mut size: usize = 0;
    let mut dev: u32 = 0;
    let mut stream: u32 = 0;
    let mut sid: [AudioStreamID; 1] = [kAudioObjectUnknown];

    adr.mSelector = kAudioDevicePropertyLatency;
    size = mem::size_of::<u32>();
    if audio_object_get_property_data(devid, &adr, &mut size, &mut dev) != NO_ERR {
        dev = 0;
    }

    adr.mSelector = kAudioDevicePropertyStreams;
    size = mem::size_of_val(&sid);
    assert_eq!(size, mem::size_of::<AudioStreamID>());
    if audio_object_get_property_data(devid, &adr, &mut size, sid.as_mut_ptr()) == NO_ERR {
        adr.mSelector = kAudioStreamPropertyLatency;
        size = mem::size_of::<u32>();
        audio_object_get_property_data(sid[0], &adr, &mut size, &mut stream);
    }

    dev + stream
}

fn audiounit_create_device_from_hwdev(dev_info: &mut ffi::cubeb_device_info, devid: AudioObjectID, devtype: DeviceType) -> Result<()>
{
    let mut adr = AudioObjectPropertyAddress {
        mSelector: 0,
        mScope: 0,
        mElement: kAudioObjectPropertyElementMaster
    };
    let mut size: usize = 0;

    if devtype == DeviceType::OUTPUT {
        adr.mScope = kAudioDevicePropertyScopeOutput;
    } else if devtype == DeviceType::INPUT {
        adr.mScope = kAudioDevicePropertyScopeInput;
    } else {
        return Err(Error::error());
    }

    let ch = audiounit_get_channel_count(devid, adr.mScope);
    if ch == 0 {
        return Err(Error::error());
    }

    // Set all data in dev_info to zero(its default data is zero):
    // https://github.com/djg/cubeb-rs/blob/78ed9459b8ac2ca50ea37bb72f8a06847eb8d379/cubeb-sys/src/device.rs#L129
    *dev_info = ffi::cubeb_device_info::default();

    let mut device_id_str: CFStringRef = ptr::null();
    size = mem::size_of::<CFStringRef>();
    adr.mSelector = kAudioDevicePropertyDeviceUID;
    let mut ret = audio_object_get_property_data(devid, &adr, &mut size, &mut device_id_str);
    if ret == NO_ERR && !device_id_str.is_null() {
        let c_string = audiounit_strref_to_cstr_utf8(device_id_str);
        // Leak the memory to the external code.
        dev_info.device_id = c_string.into_raw();

        // TODO: Why we set devid here? Does it has relationship with device_id_str?
        assert!(mem::size_of::<ffi::cubeb_devid>() >= mem::size_of_val(&devid),
                "cubeb_devid can't represent devid");
        dev_info.devid = devid as ffi::cubeb_devid;

        dev_info.group_id = dev_info.device_id;

        unsafe {
            CFRelease(device_id_str as *const c_void);
        }
        // TODO: device_id_str is a danlging pointer now.
        //       Find a way to prevent it from being used.
    }

    let mut friendly_name_str: CFStringRef = ptr::null();
    let mut ds: u32 = 0;
    size = mem::size_of::<u32>();
    adr.mSelector = kAudioDevicePropertyDataSource;
    ret = audio_object_get_property_data(devid, &adr, &mut size, &mut ds);
    if ret == NO_ERR {
        let mut trl = AudioValueTranslation {
            mInputData: &mut ds as *mut u32 as *mut c_void,
            mInputDataSize: mem::size_of_val(&ds) as u32,
            mOutputData: &mut friendly_name_str as *mut CFStringRef as *mut c_void,
            mOutputDataSize: mem::size_of::<CFStringRef>() as u32,
        };
        adr.mSelector = kAudioDevicePropertyDataSourceNameForIDCFString;
        size = mem::size_of::<AudioValueTranslation>();
        audio_object_get_property_data(devid, &adr, &mut size, &mut trl);
    }

    // If there is no datasource for this device, fall back to the
    // device name.
    if friendly_name_str.is_null() {
        size = mem::size_of::<CFStringRef>();
        adr.mSelector = kAudioObjectPropertyName;
        audio_object_get_property_data(devid, &adr, &mut size, &mut friendly_name_str);
    }

    if friendly_name_str.is_null() {
        // Couldn't get a datasource name nor a device name, return a
        // valid string of length 0.
        let c_string = CString::default();
        dev_info.friendly_name = c_string.into_raw();
    } else {
        let c_string = audiounit_strref_to_cstr_utf8(friendly_name_str);
        // Leak the memory to the external code.
        dev_info.friendly_name = c_string.into_raw();
        unsafe {
            CFRelease(friendly_name_str as *const c_void);
        }
        // TODO: friendly_name_str is a danlging pointer now.
        //       Find a way to prevent it from being used.
    };

    let mut vendor_name_str: CFStringRef = ptr::null();
    size = mem::size_of::<CFStringRef>();
    adr.mSelector = kAudioObjectPropertyManufacturer;
    ret = audio_object_get_property_data(devid, &adr, &mut size, &mut vendor_name_str);
    if ret == NO_ERR && !vendor_name_str.is_null() {
        let c_string = audiounit_strref_to_cstr_utf8(vendor_name_str);
        // Leak the memory to the external code.
        dev_info.vendor_name = c_string.into_raw();
        unsafe {
            CFRelease(vendor_name_str as *const c_void);
        }
        // TODO: vendor_name_str is a danlging pointer now.
        //       Find a way to prevent it from being used.
    }

    // TODO: Implement From trait for enum cubeb_device_type so we can use
    // `devtype.into()` to get `ffi::CUBEB_DEVICE_TYPE_*`.
    dev_info.device_type = if devtype == DeviceType::OUTPUT {
        ffi::CUBEB_DEVICE_TYPE_OUTPUT
    } else if devtype == DeviceType::INPUT {
        ffi::CUBEB_DEVICE_TYPE_INPUT
    } else {
        ffi::CUBEB_DEVICE_TYPE_UNKNOWN
    };
    dev_info.state = ffi::CUBEB_DEVICE_STATE_ENABLED;
    dev_info.preferred = if devid == audiounit_get_default_device_id(devtype) {
        ffi::CUBEB_DEVICE_PREF_ALL
    } else {
        ffi::CUBEB_DEVICE_PREF_NONE
    };

    dev_info.max_channels = ch;
    dev_info.format = ffi::CUBEB_DEVICE_FMT_ALL;
    dev_info.default_format = ffi::CUBEB_DEVICE_FMT_F32NE;
    audiounit_get_available_samplerate(devid, adr.mScope,
                                       &mut dev_info.min_rate, &mut dev_info.max_rate, &mut dev_info.default_rate);

    let latency = audiounit_get_device_presentation_latency(devid, adr.mScope);
    let mut range = AudioValueRange::default();
    adr.mSelector = kAudioDevicePropertyBufferFrameSizeRange;
    size = mem::size_of::<AudioValueRange>();
    ret = audio_object_get_property_data(devid, &adr, &mut size, &mut range);
    if ret == NO_ERR {
        dev_info.latency_lo = latency + range.mMinimum as u32;
        dev_info.latency_hi = latency + range.mMaximum as u32;
    } else {
        dev_info.latency_lo = 10 * dev_info.default_rate / 1000;    /* Default to 10ms */
        dev_info.latency_hi = 100 * dev_info.default_rate / 1000;   /* Default to 10ms */
    }

    Ok(())
}

// TODO: Rename to is_private_aggregate_device ?
//       Is it possible to have a public aggregate device ?
fn is_aggregate_device(device_info: &ffi::cubeb_device_info) -> bool
{
    // https://play.rust-lang.org/?version=stable&mode=debug&edition=2018&gist=dda40d0b40a8d922649521544f260a91
    assert!(!device_info.friendly_name.is_null());
    let private_name = CString::new(PRIVATE_AGGREGATE_DEVICE_NAME)
        .expect("Fail on creating a private name");
    unsafe {
        libc::strncmp(device_info.friendly_name, private_name.as_ptr(),
                      libc::strlen(private_name.as_ptr())) == 0
    }
}

// Retake the memory of these strings from the external code.
fn audiounit_device_destroy(device: &mut ffi::cubeb_device_info)
{
    // This should be mapped to the memory allocation in
    // audiounit_create_device_from_hwdev.
    // Set the pointers to null incase it points to some released
    // memory. (TODO: C version doesn't do this.)
    unsafe {
        if !device.device_id.is_null() {
            // group_id is a mirror to device_id, so we could skip it.
            assert!(!device.group_id.is_null());
            assert_eq!(device.device_id, device.group_id);
            let _ = CString::from_raw(device.device_id as *mut _);
            device.device_id = ptr::null();
            device.group_id = ptr::null();
        }
        if !device.friendly_name.is_null() {
            let _ = CString::from_raw(device.friendly_name as *mut _);
            device.friendly_name = ptr::null();
        }
        if !device.vendor_name.is_null() {
            let _ = CString::from_raw(device.vendor_name as *mut _);
            device.vendor_name = ptr::null();
        }
    }
}

fn audiounit_get_devices_of_type(devtype: DeviceType) -> Vec<AudioObjectID>
{
    let mut size: usize = 0;
    let mut ret = audio_object_get_property_data_size(kAudioObjectSystemObject,
                                                      &DEVICES_PROPERTY_ADDRESS,
                                                      &mut size
    );
    if ret != NO_ERR {
        return Vec::new();
    }
    /* Total number of input and output devices. */
    let mut devices: Vec<AudioObjectID> = allocate_array_by_size(size);
    ret = audio_object_get_property_data(kAudioObjectSystemObject,
                                         &DEVICES_PROPERTY_ADDRESS,
                                         &mut size,
                                         devices.as_mut_ptr(),
    );
    if ret != NO_ERR {
        return Vec::new();
    }

    // Remove the aggregate device from the list of devices (if any).
    devices.retain(|&device| {
        let name = get_device_name(device);
        if name.is_null() {
            return true;
        }
        // `rust-bindgen` doesn't support `macro`
        // so we replace `CFSTR` by `cfstringref_from_static_string`.
        let private_device = cfstringref_from_static_string(PRIVATE_AGGREGATE_DEVICE_NAME);
        unsafe {
            let found = CFStringFind(name, private_device, 0).location;
            CFRelease(private_device as *const c_void);
            // TODO: release name here ? (Sync with C version here.)
            // CFRelease(name as *const c_void);
            found == kCFNotFound
        }
    });

    // devices.retain(|&device| {
    //     let name = get_device_name(device);
    //     let private_name = CString::new(PRIVATE_AGGREGATE_DEVICE_NAME).unwrap();
    //     name != private_name
    // });

    /* Expected sorted but did not find anything in the docs. */
    devices.sort();
    if devtype.contains(DeviceType::INPUT | DeviceType::OUTPUT) {
        return devices;
    }

    // FIXIT: This is wrong. We will use output scope when devtype
    //        is unknown. Change it after C version is updated!
    let scope = if devtype == DeviceType::INPUT {
        kAudioDevicePropertyScopeInput
    } else {
        kAudioDevicePropertyScopeOutput
    };
    let mut devices_in_scope = Vec::new();
    for device in devices {
        if audiounit_get_channel_count(device, scope) > 0 {
            devices_in_scope.push(device);
        }
    }

    return devices_in_scope;
}

extern fn audiounit_collection_changed_callback(_inObjectID: AudioObjectID,
                                                _inNumberAddresses: u32,
                                                _inAddresses: *const AudioObjectPropertyAddress,
                                                inClientData: *mut c_void) -> OSStatus
{
    show_callback_info(_inObjectID, _inNumberAddresses, _inAddresses, inClientData);
    let context = inClientData as *mut AudioUnitContext;

    // Rust compilter doesn't allow a pointer to be passed across threads.
    // A hacky way to do that is to cast the pointer into a value, then
    // the value, which is actually an address, can be copied into threads.
    let ctx_ptr = context as usize;

    unsafe {
        // This can be called from inside an AudioUnit function, dispatch to another queue.
        async_dispatch((*context).serial_queue, move || {
            // The scope of `lock` is a critical section.
            let ctx = ctx_ptr as *mut AudioUnitContext;
            let _lock = AutoLock::new(&mut (*ctx).mutex);

            if (*ctx).input_collection_changed_callback.is_none() &&
               (*ctx).output_collection_changed_callback.is_none() {
                return;
            }
            if (*ctx).input_collection_changed_callback.is_some() {
                let devices = audiounit_get_devices_of_type(DeviceType::INPUT);
                /* Elements in the vector expected sorted. */
                if (*ctx).input_device_array != devices {
                    (*ctx).input_device_array = devices;
                    (*ctx).input_collection_changed_callback.unwrap()(ctx as *mut _, (*ctx).input_collection_changed_user_ptr);
                }
            }
            if (*ctx).output_collection_changed_callback.is_some() {
                let devices = audiounit_get_devices_of_type(DeviceType::OUTPUT);
                /* Elements in the vector expected sorted. */
                if (*ctx).output_device_array != devices {
                    (*ctx).output_device_array = devices;
                    (*ctx).output_collection_changed_callback.unwrap()(ctx as *mut _, (*ctx).output_collection_changed_user_ptr);
                }
            }
        });
    }

    0 // noErr.
}

fn audiounit_add_device_listener(context: *mut AudioUnitContext,
                                 devtype: DeviceType,
                                 collection_changed_callback: ffi::cubeb_device_collection_changed_callback,
                                 user_ptr: *mut c_void) -> OSStatus
{
    unsafe {
        (*context).mutex.assert_current_thread_owns();
    }
    assert!(devtype.intersects(DeviceType::INPUT | DeviceType::OUTPUT));
    // TODO: We should add an assertion here! (Sync with C verstion.)
    // assert!(collection_changed_callback.is_some());
    unsafe {
        /* Note: second register without unregister first causes 'nope' error.
         * Current implementation requires unregister before register a new cb. */
        assert!(devtype.contains(DeviceType::INPUT) && (*context).input_collection_changed_callback.is_none() ||
                devtype.contains(DeviceType::OUTPUT) && (*context).output_collection_changed_callback.is_none());

        if (*context).input_collection_changed_callback.is_none() &&
           (*context).output_collection_changed_callback.is_none() {
            let ret = audio_object_add_property_listener(kAudioObjectSystemObject,
                                                         &DEVICES_PROPERTY_ADDRESS,
                                                         audiounit_collection_changed_callback,
                                                         context as *mut c_void);
            if ret != NO_ERR {
                return ret;
            }
        }

        if devtype.contains(DeviceType::INPUT) {
            /* Expected empty after unregister. */
            assert!((*context).input_device_array.is_empty());
            (*context).input_device_array = audiounit_get_devices_of_type(DeviceType::INPUT);
            (*context).input_collection_changed_callback = collection_changed_callback;
            (*context).input_collection_changed_user_ptr = user_ptr;
        }

        if devtype.contains(DeviceType::OUTPUT) {
            /* Expected empty after unregister. */
            assert!((*context).output_device_array.is_empty());
            (*context).output_device_array = audiounit_get_devices_of_type(DeviceType::OUTPUT);
            (*context).output_collection_changed_callback = collection_changed_callback;
            (*context).output_collection_changed_user_ptr = user_ptr;
        }
    }

    0 // noErr.
}

fn audiounit_remove_device_listener(context: *mut AudioUnitContext, devtype: DeviceType) -> OSStatus
{
    unsafe {
        (*context).mutex.assert_current_thread_owns();
    }
    // TODO: We should add an assertion here! (Sync with C verstion.)
    // assert!(devtype.intersects(DeviceType::INPUT | DeviceType::OUTPUT));
    unsafe {
        if devtype.contains(DeviceType::INPUT) {
            (*context).input_collection_changed_callback = None;
            (*context).input_collection_changed_user_ptr = ptr::null_mut();
            (*context).input_device_array.clear();
        }

        if devtype.contains(DeviceType::OUTPUT) {
            (*context).output_collection_changed_callback = None;
            (*context).output_collection_changed_user_ptr = ptr::null_mut();
            (*context).output_device_array.clear();
        }

        if (*context).input_collection_changed_callback.is_some() ||
           (*context).output_collection_changed_callback.is_some() {
            return 0; // noErr.
        }
    }

    /* Note: unregister a non registered cb is not a problem, not checking. */
    audio_object_remove_property_listener(kAudioObjectSystemObject,
                                          &DEVICES_PROPERTY_ADDRESS,
                                          audiounit_collection_changed_callback,
                                          context as *mut c_void)
}

pub const OPS: Ops = capi_new!(AudioUnitContext, AudioUnitStream);

#[derive(Debug)]
pub struct AudioUnitContext {
    _ops: *const Ops,
    mutex: OwnedCriticalSection,
    active_streams: i32, // TODO: Shouldn't it be u32?
    global_latency_frames: u32,
    input_collection_changed_callback: ffi::cubeb_device_collection_changed_callback,
    input_collection_changed_user_ptr: *mut c_void,
    output_collection_changed_callback: ffi::cubeb_device_collection_changed_callback,
    output_collection_changed_user_ptr: *mut c_void,
    // Store list of devices to detect changes
    input_device_array: Vec<AudioObjectID>,
    output_device_array: Vec<AudioObjectID>,
    // The queue is asynchronously deallocated once all references to it are released
    serial_queue: dispatch_queue_t,
}

impl AudioUnitContext {
    fn new() -> Self {
        AudioUnitContext {
            _ops: &OPS as *const _,
            mutex: OwnedCriticalSection::new(),
            active_streams: 0,
            global_latency_frames: 0,
            input_collection_changed_callback: None,
            input_collection_changed_user_ptr: ptr::null_mut(),
            output_collection_changed_callback: None,
            output_collection_changed_user_ptr: ptr::null_mut(),
            input_device_array: Vec::new(),
            output_device_array: Vec::new(),
            serial_queue: create_dispatch_queue(
                DISPATCH_QUEUE_LABEL,
                DISPATCH_QUEUE_SERIAL
            )
        }
    }

    fn init(&mut self) {
        self.mutex.init();
    }
}

impl ContextOps for AudioUnitContext {
    fn init(_context_name: Option<&CStr>) -> Result<Context> {
        let mut ctx = Box::new(AudioUnitContext::new());
        ctx.init();
        Ok(unsafe { Context::from_ptr(Box::into_raw(ctx) as *mut _) })
    }

    fn backend_id(&mut self) -> &'static CStr {
        unsafe { CStr::from_ptr(b"audiounit-rust\0".as_ptr() as *const _) }
    }
    #[cfg(target_os = "ios")]
    fn max_channel_count(&mut self) -> Result<u32> {
        //TODO: [[AVAudioSession sharedInstance] maximumOutputNumberOfChannels]
        Ok(2u32)
    }
    #[cfg(not(target_os = "ios"))]
    fn max_channel_count(&mut self) -> Result<u32> {
        let mut size: usize = 0;
        let mut r = NO_ERR;
        let mut output_device_id: AudioDeviceID = kAudioObjectUnknown;
        let mut stream_format = AudioStreamBasicDescription::default();
        let stream_format_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyStreamFormat,
            mScope: kAudioDevicePropertyScopeOutput,
            mElement: kAudioObjectPropertyElementMaster
        };

        output_device_id = audiounit_get_default_device_id(DeviceType::OUTPUT);
        if output_device_id == kAudioObjectUnknown {
            return Err(Error::error());
        }

        size = mem::size_of_val(&stream_format);
        assert_eq!(size, mem::size_of::<AudioStreamBasicDescription>());

        r = audio_object_get_property_data(output_device_id,
                                           &stream_format_address,
                                           &mut size,
                                           &mut stream_format);

        if r != NO_ERR {
            cubeb_log!("AudioObjectPropertyAddress/StreamFormat rv={}", r);
            return Err(Error::error());
        }

        Ok(stream_format.mChannelsPerFrame)
    }
    #[cfg(target_os = "ios")]
    fn min_latency(&mut self, _params: StreamParams) -> Result<u32> {
        Err(not_supported());
    }
    #[cfg(not(target_os = "ios"))]
    fn min_latency(&mut self, _params: StreamParams) -> Result<u32> {
        let mut latency_range = AudioValueRange::default();
        if let Err(_) = audiounit_get_acceptable_latency_range(&mut latency_range) {
            cubeb_log!("Could not get acceptable latency range.");
            return Err(Error::error()); // TODO: return the error we get instead?
        }

        Ok(cmp::max(latency_range.mMinimum as u32,
                    SAFE_MIN_LATENCY_FRAMES))
    }
    #[cfg(target_os = "ios")]
    fn preferred_sample_rate(&mut self) -> Result<u32> {
        Err(not_supported());
    }
    #[cfg(not(target_os = "ios"))]
    fn preferred_sample_rate(&mut self) -> Result<u32> {
        let mut size: usize = 0;
        let mut r = NO_ERR;
        let mut fsamplerate: f64 = 0.0;
        let mut output_device_id: AudioDeviceID = kAudioObjectUnknown;
        let samplerate_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyNominalSampleRate,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMaster
        };

        output_device_id = audiounit_get_default_device_id(DeviceType::OUTPUT);
        if output_device_id == kAudioObjectUnknown {
            return Err(Error::error());
        }

        size = mem::size_of_val(&fsamplerate);
        assert_eq!(size, mem::size_of::<f64>());
        r = audio_object_get_property_data(output_device_id,
                                           &samplerate_address,
                                           &mut size,
                                           &mut fsamplerate);

        if r != NO_ERR {
            return Err(Error::error());
        }

        Ok(fsamplerate as u32)
    }
    fn enumerate_devices(
        &mut self,
        devtype: DeviceType,
        collection: &DeviceCollectionRef,
    ) -> Result<()> {
        let mut input_devs = Vec::<AudioObjectID>::new();
        let mut output_devs = Vec::<AudioObjectID>::new();

        // Count number of input and output devices.  This is not
        // necessarily the same as the count of raw devices supported by the
        // system since, for example, with Soundflower installed, some
        // devices may report as being both input *and* output and cubeb
        // separates those into two different devices.

        if devtype.contains(DeviceType::OUTPUT) {
            output_devs = audiounit_get_devices_of_type(DeviceType::OUTPUT);
        }

        if devtype.contains(DeviceType::INPUT) {
            input_devs = audiounit_get_devices_of_type(DeviceType::INPUT);
        }

        let mut devices: Vec<ffi::cubeb_device_info> = allocate_array(
            output_devs.len() + input_devs.len()
        );

        let mut count = 0;
        if devtype.contains(DeviceType::OUTPUT) {
            for dev in output_devs {
                let device = &mut devices[count];
                if audiounit_create_device_from_hwdev(device, dev, DeviceType::OUTPUT).is_err() ||
                   is_aggregate_device(device) {
                    continue;
                }
                count += 1;
            }
        }

        if devtype.contains(DeviceType::INPUT) {
            for dev in input_devs {
                let device = &mut devices[count];
                if audiounit_create_device_from_hwdev(device, dev, DeviceType::INPUT).is_err() ||
                   is_aggregate_device(device) {
                    continue;
                }
                count += 1;
            }
        }

        // Remove the redundant space, set len to count.
        devices.truncate(count);

        let coll = unsafe { &mut *collection.as_ptr() };
        if count > 0 {
            let (ptr, len) = leak_vec(devices);
            coll.device = ptr;
            coll.count = len;
        } else {
            coll.device = ptr::null_mut();
            coll.count = 0;
        }

        Ok(())
    }
    fn device_collection_destroy(&mut self, collection: &mut DeviceCollectionRef) -> Result<()> {
        let coll = unsafe { &mut *collection.as_ptr() };
        if coll.device.is_null() {
            return Ok(());
        }

        // Retake the ownership of the previous leaked memory from the external code.
        let mut devices = retake_leaked_vec(coll.device, coll.count);
        for device in &mut devices {
            // This should be mapped to the memory allocation in
            // audiounit_create_device_from_hwdev.
            audiounit_device_destroy(device);
        }
        drop(devices); // Release the memory.
        coll.device = ptr::null_mut();
        coll.count = 0;
        Ok(())
    }
    fn stream_init(
        &mut self,
        _stream_name: Option<&CStr>,
        input_device: DeviceId,
        input_stream_params: Option<&StreamParamsRef>,
        output_device: DeviceId,
        output_stream_params: Option<&StreamParamsRef>,
        latency_frames: u32,
        data_callback: ffi::cubeb_data_callback,
        state_callback: ffi::cubeb_state_callback,
        user_ptr: *mut c_void,
    ) -> Result<Stream> {
        // TODO: Check stm.input_stream_params and stm.output_stream_params
        //       are valid and matched ? The code can easily fail if
        //       {input, output}_stream_params is
        //       ffi::cubeb_stream_params::default().
        //       (I added some easy checks in `audio_stream_desc_init` to prevent
        //        the wrong values are set.)
        //   1. What if the `stm.output_stream_params.format()` is different
        //      from `stm.input_stream_params.format()` ?
        //   2. What if the channels is different from the channels for the
        //      layout ?
        //   3. Should stm.output_stream_params.layout() always be undefined ?
        //   4. In C version. we always call `state_callback` without checking
        //      if it's null or not. It's better to add an assert here to check
        //      state_callback is some!

        // Since we cannot call `AutoLock::new(&mut self.mutex)` and
        // `AudioUnitStream::new(self, ...)` at the same time.
        // (`self` cannot be borrowed immutably after it's borrowed as mutable.),
        // we take the pointer to `self.mutex` first and then dereference it to
        // the mutex to avoid this problem for now.
        let mutex_ptr = &mut self.mutex as *mut OwnedCriticalSection;
        // The scope of `_context_lock` is a critical section.
        let _context_lock = AutoLock::new(unsafe { &mut (*mutex_ptr) });
        audiounit_increment_active_streams(self);
        let mut boxed_stream = Box::new(
            AudioUnitStream::new(
                self,
                user_ptr,
                data_callback,
                state_callback,
                latency_frames
            )
        );
        boxed_stream.init();
        // TODO: Shouldn't this be put at the first so we don't need to perform
        //       any action if the check fails? (Sync with C version)
        assert!(latency_frames > 0);
        // TODO: Shouldn't this be put at the first so we don't need to perform
        //       any action if the check fails? (Sync with C version)
        if (!input_device.is_null() && input_stream_params.is_none()) ||
           (!output_device.is_null() && output_stream_params.is_none()) {
            return Err(Error::invalid_parameter());
        }
        // TODO: Add a method `to_owned` in `StreamParamsRef`.
        if let Some(stream_params_ref) = input_stream_params {
            assert!(!stream_params_ref.as_ptr().is_null());
            boxed_stream.input_stream_params = StreamParams::from(unsafe { (*stream_params_ref.as_ptr()) });
            if let Err(r) = audiounit_set_device_info(boxed_stream.as_mut(), input_device as AudioDeviceID, DeviceType::INPUT) {
                cubeb_log!("({:p}) Fail to set device info for input.", boxed_stream.as_ref());
                return Err(r);
            }
        }
        if let Some(stream_params_ref) = output_stream_params {
            assert!(!stream_params_ref.as_ptr().is_null());
            boxed_stream.output_stream_params = StreamParams::from(unsafe { *(stream_params_ref.as_ptr()) });
            if let Err(r) = audiounit_set_device_info(boxed_stream.as_mut(), output_device as AudioDeviceID, DeviceType::OUTPUT) {
                cubeb_log!("({:p}) Fail to set device info for output.", boxed_stream.as_ref());
                return Err(r);
            }
        }

        if let Err(r) = {
            // It's not critical to lock here, because no other thread has been started
            // yet, but it allows to assert that the lock has been taken in
            // `audiounit_setup_stream`.

            // Since we cannot borrow boxed_stream as mutable twice
            // (for boxed_stream.mutex and boxed_stream itself), we store
            // the pointer to boxed_stream.mutex(it's a value) and convert it
            // to a reference as the workaround to borrow as mutable twice.
            // Same as what we did above for AudioUnitContext.mutex.
            let mutex_ptr = &mut boxed_stream.mutex as *mut OwnedCriticalSection;
            // The scope of `_lock` is a critical section.
            let _lock = AutoLock::new(unsafe { &mut (*mutex_ptr) });
            audiounit_setup_stream(boxed_stream.as_mut())
        } {
            cubeb_log!("({:p}) Could not setup the audiounit stream.", boxed_stream.as_ref());
            return Err(r);
        }

        if let Err(r) = audiounit_install_system_changed_callback(boxed_stream.as_mut()) {
            cubeb_log!("({:p}) Could not install the device change callback.", boxed_stream.as_ref());
            return Err(r);
        }

        println!("<Initialize> stream @ {:p}\nstream.context @ {:p}\n{:?}",
                 boxed_stream.as_ref(), boxed_stream.context, boxed_stream.as_ref());
        let cubeb_stream = unsafe {
            Stream::from_ptr(Box::into_raw(boxed_stream) as *mut _)
        };
        Ok(cubeb_stream)
    }
    fn register_device_collection_changed(
        &mut self,
        devtype: DeviceType,
        collection_changed_callback: ffi::cubeb_device_collection_changed_callback,
        user_ptr: *mut c_void,
    ) -> Result<()> {
        if devtype == DeviceType::UNKNOWN {
            return Err(Error::invalid_parameter());
        }
        let mut ret = NO_ERR;
        let ctx_ptr = self as *mut AudioUnitContext;
        // The scope of `lock` is a critical section.
        let _lock = AutoLock::new(&mut self.mutex);
        if collection_changed_callback.is_some() {
            ret = audiounit_add_device_listener(ctx_ptr,
                                                devtype,
                                                collection_changed_callback,
                                                user_ptr);
        } else {
            ret = audiounit_remove_device_listener(ctx_ptr, devtype);
        }
        if ret == NO_ERR {
            Ok(())
        } else {
            Err(Error::error())
        }
    }
}

impl Drop for AudioUnitContext {
    fn drop(&mut self) {
        println!("Drop context @ {:p}", self);
    }
}

#[derive(Debug)]
struct AudioUnitStream<'ctx> {
    context: &'ctx mut AudioUnitContext,
    user_ptr: *mut c_void,

    data_callback: ffi::cubeb_data_callback,
    state_callback: ffi::cubeb_state_callback,
    device_changed_callback: ffi::cubeb_device_changed_callback,
    device_changed_callback_lock: OwnedCriticalSection,
    /* Stream creation parameters */
    input_stream_params: StreamParams,
    output_stream_params: StreamParams,
    input_device: device_info,
    output_device: device_info,
    /* Format descriptions */
    input_desc: AudioStreamBasicDescription,
    output_desc: AudioStreamBasicDescription,
    /* I/O AudioUnits */
    input_unit: AudioUnit,
    output_unit: AudioUnit,
    /* I/O device sample rate */
    input_hw_rate: f64,
    output_hw_rate: f64,
    /* Expected I/O thread interleave,
     * calculated from I/O hw rate. */
    expected_output_callbacks_in_a_row: i32,
    mutex: OwnedCriticalSection,
    // Hold the input samples in every input callback iteration.
    // Only accessed on input/output callback thread and during initial configure.
    input_linear_buffer: Option<Box<AutoArrayWrapper>>,
    /* Frame counters */
    frames_played: AtomicU64,
    // How many frames got read from the input since the stream started (includes
    // padded silence)
    frames_read: AtomicI64,
    shutdown: AtomicBool,
    draining: AtomicBool,
    reinit_pending: AtomicBool,
    destroy_pending: AtomicBool,
    /* Latency requested by the user. */
    latency_frames: u32,
    current_latency_frames: AtomicU32,
    panning: atomic::Atomic<f32>,
    resampler: AutoRelease<ffi::cubeb_resampler>,
    /* This is true if a device change callback is currently running.  */
    switching_device: AtomicBool,
    buffer_size_change_state: AtomicBool,
    aggregate_device_id: AudioDeviceID, // the aggregate device id
    plugin_id: AudioObjectID,           // used to create aggregate device
    /* Listeners indicating what system events are monitored. */
    default_input_listener: Option<property_listener<'ctx>>,
    default_output_listener: Option<property_listener<'ctx>>,
    input_alive_listener: Option<property_listener<'ctx>>,
    input_source_listener: Option<property_listener<'ctx>>,
    output_source_listener: Option<property_listener<'ctx>>,
}

impl<'ctx> AudioUnitStream<'ctx> {
    fn new(
        context: &'ctx mut AudioUnitContext,
        user_ptr: *mut c_void,
        data_callback: ffi::cubeb_data_callback,
        state_callback: ffi::cubeb_state_callback,
        latency_frames: u32,
    ) -> Self {
        AudioUnitStream {
            context,
            user_ptr,
            data_callback,
            state_callback,
            device_changed_callback: None,
            device_changed_callback_lock: OwnedCriticalSection::new(),
            input_stream_params: StreamParams::from(
                ffi::cubeb_stream_params {
                    format: ffi::CUBEB_SAMPLE_FLOAT32NE,
                    rate: 0,
                    channels: 0,
                    layout: ffi::CUBEB_LAYOUT_UNDEFINED,
                    prefs: ffi::CUBEB_STREAM_PREF_NONE
                }
            ),
            output_stream_params: StreamParams::from(
                ffi::cubeb_stream_params {
                    format: ffi::CUBEB_SAMPLE_FLOAT32NE,
                    rate: 0,
                    channels: 0,
                    layout: ffi::CUBEB_LAYOUT_UNDEFINED,
                    prefs: ffi::CUBEB_STREAM_PREF_NONE
                }
            ),
            input_device: device_info::new(),
            output_device: device_info::new(),
            input_desc: AudioStreamBasicDescription::default(),
            output_desc: AudioStreamBasicDescription::default(),
            input_unit: ptr::null_mut(),
            output_unit: ptr::null_mut(),
            input_hw_rate: 0_f64,
            output_hw_rate: 0_f64,
            expected_output_callbacks_in_a_row: 0,
            mutex: OwnedCriticalSection::new(),
            input_linear_buffer: None,
            frames_played: AtomicU64::new(0),
            frames_read: AtomicI64::new(0),
            shutdown: AtomicBool::new(true),
            draining: AtomicBool::new(false),
            reinit_pending: AtomicBool::new(false),
            destroy_pending: AtomicBool::new(false),
            latency_frames,
            current_latency_frames: AtomicU32::new(0),
            panning: atomic::Atomic::new(0.0_f32),
            resampler: AutoRelease::new(ptr::null_mut(), ffi::cubeb_resampler_destroy),
            switching_device: AtomicBool::new(false),
            buffer_size_change_state: AtomicBool::new(false),
            // TODO: C version uses 0 instead.
            aggregate_device_id: kAudioObjectUnknown,
            plugin_id: 0,
            default_input_listener: None,
            default_output_listener: None,
            input_alive_listener: None,
            input_source_listener: None,
            output_source_listener: None,
        }
    }

    fn init(&mut self) {
        self.device_changed_callback_lock.init();
        self.mutex.init();
    }

    fn destroy(&mut self) {
        audiounit_stream_destroy(self);
    }
}

impl<'ctx> Drop for AudioUnitStream<'ctx> {
    fn drop(&mut self) {
        self.destroy();
        println!("<Drop> stream @ {:p}\nstream.context @ {:p}\n{:?}",
                 self, self.context, self);
    }
}

impl<'ctx> StreamOps for AudioUnitStream<'ctx> {
    fn start(&mut self) -> Result<()> {
        // The scope of `_context_lock` is a critical section.
        // Use `mutex_ptr` to avoid the borrowing twice issue.
        let mutex_ptr = &mut self.context.mutex as *mut OwnedCriticalSection;
        let _context_lock = AutoLock::new(unsafe { &mut (*mutex_ptr) });

        *self.shutdown.get_mut() = false;
        *self.draining.get_mut() = false;

        audiounit_stream_start_internal(self);

        // TODO: C version doesn't check if state_callback is a null pointer.
        if self.state_callback.is_some() {
            unsafe {
                (self.state_callback.unwrap())(
                    self as *mut AudioUnitStream as *mut ffi::cubeb_stream,
                    self.user_ptr,
                    ffi::CUBEB_STATE_STARTED);
            }
        }

        cubeb_log!("Cubeb stream ({:p}) started successfully.", self);
        Ok(())
    }
    fn stop(&mut self) -> Result<()> {
        // The scope of `_context_lock` is a critical section.
        // Use `mutex_ptr` to avoid the borrowing twice issue.
        let mutex_ptr = &mut self.context.mutex as *mut OwnedCriticalSection;
        let _context_lock = AutoLock::new(unsafe { &mut (*mutex_ptr) });

        *self.shutdown.get_mut() = true;

        audiounit_stream_stop_internal(self);

        // TODO: C version doesn't check if state_callback is a null pointer.
        if self.state_callback.is_some() {
            unsafe {
                (self.state_callback.unwrap())(
                    self as *mut AudioUnitStream as *mut ffi::cubeb_stream,
                    self.user_ptr,
                    ffi::CUBEB_STATE_STOPPED
                );
            }
        }

        cubeb_log!("Cubeb stream ({:p}) stopped successfully.", self);
        Ok(())
    }
    fn reset_default_device(&mut self) -> Result<()> {
        Ok(())
    }
    fn position(&mut self) -> Result<u64> {
        let position = if *self.current_latency_frames.get_mut() as u64 > *self.frames_played.get_mut() {
            0
        } else {
            *self.frames_played.get_mut() - *self.current_latency_frames.get_mut() as u64
        };
        Ok(position)
    }
    #[cfg(target_os = "ios")]
    fn latency(&mut self) -> Result<u32> {
        Err(not_supported())
    }
    #[cfg(not(target_os = "ios"))]
    fn latency(&mut self) -> Result<u32> {
        Ok(self.current_latency_frames.load(Ordering::SeqCst))
    }
    fn set_volume(&mut self, volume: f32) -> Result<()> {
        assert!(!self.output_unit.is_null());
        let mut r = NO_ERR;
        r = audio_unit_set_parameter(self.output_unit,
                                     kHALOutputParam_Volume,
                                     kAudioUnitScope_Global,
                                     0, volume, 0);
        if r != NO_ERR {
            cubeb_log!("AudioUnitSetParameter/kHALOutputParam_Volume rv={}", r);
            return Err(Error::error());
        }
        Ok(())
    }
    fn set_panning(&mut self, panning: f32) -> Result<()> {
        if self.output_desc.mChannelsPerFrame > 2 {
            return Err(Error::invalid_format());
        }

        self.panning.store(panning, Ordering::Relaxed);
        Ok(())
    }
    #[cfg(target_os = "ios")]
    fn current_device(&mut self) -> Result<&DeviceRef> {
        Err(not_supported())
    }
    #[cfg(not(target_os = "ios"))]
    fn current_device(&mut self) -> Result<&DeviceRef> {
        let mut device: Box<ffi::cubeb_device> = Box::new(unsafe { mem::zeroed() });
        audiounit_get_default_device_name(self, device.as_mut(), DeviceType::OUTPUT)?;
        audiounit_get_default_device_name(self, device.as_mut(), DeviceType::INPUT)?;
        Ok(unsafe { DeviceRef::from_ptr(Box::into_raw(device) as *mut _) })
    }
    #[cfg(target_os = "ios")]
    fn device_destroy(&mut self, device: &DeviceRef) -> Result<()> {
        Err(not_supported())
    }
    #[cfg(not(target_os = "ios"))]
    fn device_destroy(&mut self, device: &DeviceRef) -> Result<()> {
        if device.as_ptr().is_null() {
            Err(Error::error())
        } else {
            unsafe {
                let mut dev: Box<ffi::cubeb_device> = Box::from_raw(device.as_ptr() as *mut _);
                if !dev.output_name.is_null() {
                    let _ = CString::from_raw(dev.output_name as *mut _);
                    dev.output_name = ptr::null_mut();
                }
                if !dev.input_name.is_null() {
                    let _ = CString::from_raw(dev.input_name as *mut _);
                    dev.input_name = ptr::null_mut();
                }
                drop(dev);
            }
            Ok(())
        }
    }
    fn register_device_changed_callback(
        &mut self,
        device_changed_callback: ffi::cubeb_device_changed_callback,
    ) -> Result<()> {
        // The scope of `_dev_cb_lock` is a critical section.
        let _dev_cb_lock = AutoLock::new(&mut self.device_changed_callback_lock);
        /* Note: second register without unregister first causes 'nope' error.
         * Current implementation requires unregister before register a new cb. */
        // TODO: The above comment is wrong. We cannot unregister the original
        //       callback since we will hit the following assertion!
        //       A less strict assertion works as what the comment want is
        //       something like:
        // assert!(device_changed_callback.is_none() || self.device_changed_callback.is_none());
        // assert_eq!(self.device_changed_callback, None);
        self.device_changed_callback = device_changed_callback;
        Ok(())
    }
}

#[cfg(test)]
mod test;

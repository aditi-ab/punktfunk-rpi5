//! GPU smoke test — `#[ignore]`d because it needs real Vulkan Video hardware.
//!
//! Run on a Vulkan-Video box with:
//!
//! ```text
//! cargo test -p pf-vkdecode -- --ignored
//! ```
//!
//! Environment expectations (the fleet's RADV boxes .21/.25, the NVIDIA .173, or
//! any machine like them):
//! - a Vulkan 1.3 loader on the library path (`libvulkan.so.1` / `vulkan-1.dll`);
//! - a physical device advertising `VK_KHR_video_queue`,
//!   `VK_KHR_video_decode_queue` and `VK_KHR_video_decode_h264`, with a queue
//!   family carrying `VIDEO_DECODE_KHR` ops for H.264;
//! - `timelineSemaphore` + `synchronization2` feature support (Vulkan 1.3 core).
//!
//! What it proves: device wrap → caps query/derivation on REAL caps → session +
//! parameters creation → DPB/output/ring pools → 48 AUs of the vendored 25fps
//! vector decoded through `vkCmdDecodeVideoKHR` — well past DPB-full, so the
//! bump-eviction slot-reuse path runs — with the full frame lifecycle each
//! delivery: `wait_status` reading the RESULT_STATUS_ONLY query back as
//! COMPLETE, then `release_frame` returning the slot (the two-phase release the
//! coincide-mode overwrite fix depends on). What it deliberately does NOT prove
//! (WP-D on-glass): pixel correctness vs the ffmpeg rung, presenter
//! interop/layout round-trips, soak, and both vendors' DPB arrangements at once
//! (each box exercises only its own).

#![deny(clippy::undocumented_unsafe_blocks)]

use std::io::Cursor;

use ash::vk;
use ash::vk::Handle;
use cros_codecs::codec::h264::parser::Nalu;
use cros_codecs::codec::h264::parser::NaluType;
use pf_vkdecode::DecodeStatus;
use pf_vkdecode::DeviceHandles;
use pf_vkdecode::NoopQueueLock;
use pf_vkdecode::VkH264Decoder;

// The same vendored vector the WP-A tests convert, same relative path.
const TEST_25FPS: &[u8] = include_bytes!(
    "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
);

/// Test-only AU splitter, mirroring pf-bitstream's (`#[cfg(test)]`-private there).
fn split_into_aus(stream: &[u8]) -> Vec<&[u8]> {
    let mut aus = Vec::new();
    let mut cursor = Cursor::new(stream);
    let mut au_start = 0usize;
    let mut au_has_slice = false;

    while let Ok(nalu) = Nalu::next(&mut cursor) {
        let nalu_offset = cursor.position() as usize;
        let start = nalu_offset - nalu.offset;
        let is_slice = matches!(nalu.header.type_, NaluType::Slice | NaluType::SliceIdr);
        let first_mb_zero = is_slice && stream.get(nalu_offset + 1).is_some_and(|b| b & 0x80 != 0);

        if au_has_slice && (!is_slice || first_mb_zero) {
            aus.push(&stream[au_start..start]);
            au_start = start;
            au_has_slice = false;
        }
        au_has_slice |= is_slice;
    }
    aus.push(&stream[au_start..]);
    aus
}

#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn decodes_48_aus_with_status_reads_and_frame_releases_past_dpb_full() {
    // ---- instance ----
    // SAFETY: loads the system Vulkan loader; no Vulkan objects exist yet.
    let entry = unsafe { ash::Entry::load() }.expect("a Vulkan loader on this box");
    let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 3, 0));
    let instance_ci = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: valid create info rooted in locals; the instance is destroyed at the
    // end of this test after everything created from it.
    let instance =
        unsafe { entry.create_instance(&instance_ci, None) }.expect("create a Vulkan 1.3 instance");

    // ---- physical device with an H.264 decode queue family ----
    // SAFETY: live instance.
    let physical_devices =
        unsafe { instance.enumerate_physical_devices() }.expect("enumerate physical devices");
    let mut picked: Option<(vk::PhysicalDevice, u32, u32)> = None;
    for pd in physical_devices {
        // SAFETY: `pd` was just enumerated from this instance.
        let ext_props =
            unsafe { instance.enumerate_device_extension_properties(pd) }.unwrap_or_default();
        let has = |name: &std::ffi::CStr| {
            ext_props.iter().any(|e| {
                e.extension_name_as_c_str()
                    .is_ok_and(|extension| extension == name)
            })
        };
        if !(has(ash::khr::video_queue::NAME)
            && has(ash::khr::video_decode_queue::NAME)
            && has(ash::khr::video_decode_h264::NAME))
        {
            continue;
        }
        // SAFETY: live physical device; the two-call form fills the chained video
        // properties for each family.
        let family_count = unsafe { instance.get_physical_device_queue_family_properties2_len(pd) };
        let mut video_props = vec![vk::QueueFamilyVideoPropertiesKHR::default(); family_count];
        let mut families: Vec<vk::QueueFamilyProperties2<'_>> = video_props
            .iter_mut()
            .map(|v| vk::QueueFamilyProperties2::default().push_next(v))
            .collect();
        // SAFETY: as above, arrays sized to the reported count.
        unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut families) };
        let flags_per_family: Vec<vk::QueueFlags> = families
            .iter()
            .map(|f| f.queue_family_properties.queue_flags)
            .collect();
        drop(families); // release the &mut borrows so video_props is readable

        let mut decode_qf = None;
        let mut graphics_qf = None;
        for (index, flags) in flags_per_family.iter().enumerate() {
            if flags.contains(vk::QueueFlags::GRAPHICS) && graphics_qf.is_none() {
                graphics_qf = Some(index as u32);
            }
            if flags.contains(vk::QueueFlags::VIDEO_DECODE_KHR)
                && video_props[index]
                    .video_codec_operations
                    .contains(vk::VideoCodecOperationFlagsKHR::DECODE_H264)
                && decode_qf.is_none()
            {
                decode_qf = Some(index as u32);
            }
        }
        if let Some(decode) = decode_qf {
            picked = Some((pd, decode, graphics_qf.unwrap_or(decode)));
            break;
        }
    }
    let (pd, decode_qf, graphics_qf) =
        picked.expect("a physical device with VK_KHR_video_decode_h264 and a decode queue");

    // ---- logical device: decode (+ graphics) queues, video + sync features ----
    let priorities = [1.0f32];
    let mut queue_infos = vec![vk::DeviceQueueCreateInfo::default()
        .queue_family_index(decode_qf)
        .queue_priorities(&priorities)];
    if graphics_qf != decode_qf {
        queue_infos.push(
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(graphics_qf)
                .queue_priorities(&priorities),
        );
    }
    let extensions = [
        ash::khr::video_queue::NAME.as_ptr(),
        ash::khr::video_decode_queue::NAME.as_ptr(),
        ash::khr::video_decode_h264::NAME.as_ptr(),
    ];
    let mut features12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
    let mut features13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
    let device_ci = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_infos)
        .enabled_extension_names(&extensions)
        .push_next(&mut features12)
        .push_next(&mut features13);
    // SAFETY: live physical device, valid create info rooted in locals; destroyed
    // at the end of this test after the decoder drops.
    let device =
        unsafe { instance.create_device(pd, &device_ci, None) }.expect("create the decode device");

    // ---- the decoder over borrowed handles, exactly as WP-C will hold it ----
    let handles = DeviceHandles {
        get_instance_proc_addr: entry.static_fn().get_instance_proc_addr as usize,
        instance: instance.handle().as_raw() as usize,
        physical_device: pd.as_raw() as usize,
        device: device.handle().as_raw() as usize,
        decode_qf,
        decode_queue_index: 0,
        graphics_qf,
    };
    {
        // SAFETY: the handles above are live for this whole block (the decoder
        // drops at its end, before the device/instance destroys below), the device
        // was created with the decode extensions + timeline/sync2 features, and
        // the queue fields name the families/queues created above.
        let mut decoder = unsafe { VkH264Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");

        // 48 AUs — far past the vector's DPB depth, so evicted slots recycle
        // repeatedly — with WP-C's one-in/one-out lifecycle on every delivered
        // frame: wait its status (the program's whole point: the driver must
        // say COMPLETE, per op) and release it so its slot may host a later
        // decode. Output lags decode by a couple of AUs (B-pictures), so the
        // delivered count is asserted with slack.
        let aus = split_into_aus(TEST_25FPS);
        let mut delivered = 0usize;
        let mut geometry_checked = false;
        for au in aus.iter().take(48) {
            let mut next = decoder
                .decode(au)
                .expect("decode an AU of the clean vector");
            while let Some(frame) = next {
                if !geometry_checked {
                    assert_eq!((frame.coded_width, frame.coded_height), (320, 240));
                    assert_eq!(
                        (frame.crop.width, frame.crop.height),
                        (320, 240),
                        "the vector is uncropped"
                    );
                    assert_ne!(frame.image, vk::Image::null());
                    assert_ne!(frame.semaphore, vk::Semaphore::null());
                    assert!(frame.value > 0);
                    geometry_checked = true;
                }
                assert_eq!(
                    decoder.wait_status(&frame),
                    DecodeStatus::Ok,
                    "the driver must report every decode op COMPLETE"
                );
                decoder
                    .release_frame(&frame)
                    .expect("a current-generation frame releases");
                delivered += 1;
                next = decoder.take_ready();
            }
        }
        assert!(
            delivered >= 40,
            "expected at least 40 delivered frames from 48 AUs, got {delivered}"
        );
    }

    // ---- teardown (decoder is gone; its Drop drained the queue) ----
    // SAFETY: every object created from the device (the decoder's pools/session)
    // was destroyed when the decoder dropped above.
    unsafe {
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
}

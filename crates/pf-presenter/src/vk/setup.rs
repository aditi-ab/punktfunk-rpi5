//! Presenter bring-up: instance → surface → device → swapchain (init-time construction).

#[cfg(target_os = "linux")]
use super::HwCtx;
#[cfg(windows)]
use super::HwCtxWin;
use super::{OverlayPipe, Presenter};
use crate::csc::CscPass;
#[cfg(target_os = "linux")]
use crate::dmabuf;
use anyhow::{anyhow, bail, Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
use std::ffi::{c_char, CString};

impl Presenter {
    /// Bring up instance → surface → device → swapchain over an SDL window.
    /// `instance_extensions` comes from `VideoSubsystem::vulkan_instance_extensions()`.
    pub fn new(
        window: &sdl3::video::Window,
        instance_extensions: &[String],
        pref: PresentPref,
    ) -> Result<Presenter> {
        // SAFETY: per the Vulkan contract above - a create/allocate call on the live device, over
        // builder structs that are locals outliving the call; the handle it returns is owned by
        // the value being built here.
        let entry = unsafe { ash::Entry::load() }.context("libvulkan not loadable")?;

        let app_name = CString::new("punktfunk-session").unwrap();
        // 1.3: FFmpeg's Vulkan hwcontext requires an instance of at least 1.3 (any
        // current loader accepts it regardless of device support; device-level gating
        // happens below).
        let app_info = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .api_version(vk::API_VERSION_1_3);
        // HDR10 presentation needs the extended colorspaces at the INSTANCE level.
        let mut instance_extensions: Vec<String> = instance_extensions.to_vec();
        let inst_available =
            // SAFETY: per the Vulkan contract above - a read-only query on the live
            // instance/device, filling locals returned by value.
            unsafe { entry.enumerate_instance_extension_properties(None) }.unwrap_or_default();
        let has_colorspace_ext = inst_available
            .iter()
            .any(|e| e.extension_name_as_c_str() == Ok(c"VK_EXT_swapchain_colorspace"));
        if has_colorspace_ext {
            instance_extensions.push("VK_EXT_swapchain_colorspace".into());
        }
        let ext_cstrings: Vec<CString> = instance_extensions
            .iter()
            .map(|e| CString::new(e.as_str()).unwrap())
            .collect();
        // `c_char`, not `i8`: plain `char` is SIGNED on x86_64 but UNSIGNED on aarch64, so a
        // hardcoded `*const i8` compiles on the desktop targets and fails to match ash's
        // `&[*const c_char]` on ARM.
        let ext_ptrs: Vec<*const c_char> = ext_cstrings.iter().map(|e| e.as_ptr()).collect();
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app_info)
                    .enabled_extension_names(&ext_ptrs),
                None,
            )
        }
        .context("vkCreateInstance")?;
        let surface_i = ash::khr::surface::Instance::new(&entry, &instance);

        // SAFETY: per the Vulkan contract above - a create/allocate call on the live device, over
        // builder structs that are locals outliving the call; the handle it returns is owned by
        // the value being built here.
        let surface = unsafe { window.vulkan_create_surface(instance.handle()) }
            .map_err(|e| anyhow!("SDL_Vulkan_CreateSurface: {e}"))?;

        let (pdev, qfi) = pick_device(&instance, &surface_i, surface)?;
        // SAFETY: per the Vulkan contract above - a read-only query on the live instance/device,
        // filling locals returned by value.
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pdev) };
        {
            // SAFETY: per the Vulkan contract above - a read-only query on the live
            // instance/device, filling locals returned by value.
            let props = unsafe { instance.get_physical_device_properties(pdev) };
            let name = props
                .device_name_as_c_str()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default();
            tracing::info!(device = %name, queue_family = qfi, "vulkan device");
        }

        // The dmabuf import set is optional: enabled when the device offers all four,
        // else that path is off (`supports_dmabuf() == false`). Windows has no
        // dmabuf/DRM-PRIME — the whole import path is compiled out there.
        // SAFETY: per the Vulkan contract above - a read-only query on the live instance/device,
        // filling locals returned by value.
        let available = unsafe { instance.enumerate_device_extension_properties(pdev) }?;
        let has = |name: &std::ffi::CStr| {
            available
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(name))
        };
        #[cfg(target_os = "linux")]
        let hw_capable = dmabuf::DEVICE_EXTENSIONS.iter().all(|n| has(n));
        let mut dev_exts = vec![ash::khr::swapchain::NAME.as_ptr()];
        #[cfg(target_os = "linux")]
        if hw_capable {
            dev_exts.extend(dmabuf::DEVICE_EXTENSIONS.iter().map(|n| n.as_ptr()));
        } else {
            tracing::info!(
                "device lacks the dmabuf import extensions — VAAPI hardware frames \
                 unavailable"
            );
        }
        // D3D11 shared-texture import (the D3D11VA decode hand-off) — optional exactly
        // like the dmabuf set; a device without it keeps Vulkan-Video/software decode.
        // Extensions alone aren't the whole gate: the driver must also report the
        // multiplanar NV12 image as IMPORTABLE from a D3D11 texture handle
        // (vkGetPhysicalDeviceImageFormatProperties2 — creating an unsupported external
        // image is UB, observed as VK_ERROR_DEVICE_LOST at the first submits on NVIDIA).
        #[cfg(windows)]
        let (import_bgra8, import_rgb10) = crate::d3d11::import_supported(&instance, pdev);
        #[cfg(windows)]
        let win_capable = crate::d3d11::DEVICE_EXTENSIONS.iter().all(|n| has(n)) && import_bgra8;
        #[cfg(windows)]
        if win_capable {
            dev_exts.extend(crate::d3d11::DEVICE_EXTENSIONS.iter().map(|n| n.as_ptr()));
        } else {
            tracing::info!(
                "device lacks the win32 external-memory/keyed-mutex extensions — D3D11VA \
                 hardware frames unavailable"
            );
        }
        // The adapter LUID (for the D3D11VA backend to create its decode device on the
        // SAME adapter). Core 1.1 query; valid on effectively every Windows driver.
        let mut id_props = vk::PhysicalDeviceIDProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id_props);
        // SAFETY: per the Vulkan contract above - a read-only query on the live instance/device,
        // filling locals returned by value.
        unsafe { instance.get_physical_device_properties2(pdev, &mut props2) };
        let adapter_luid: Option<[u8; 8]> =
            (id_props.device_luid_valid == vk::TRUE).then_some(id_props.device_luid);
        // Static HDR metadata (ST.2086 mastering + CLL) to the presentation engine.
        // Compositors key their "this app is HDR" signaling on the client pushing
        // metadata via vkSetHdrMetadataEXT in addition to picking the HDR10 colorspace
        // (gamescope's SteamOS HDR badge and per-app tone-map targets among them) —
        // the colorspace alone leaves the app looking SDR to the shell.
        let has_hdr_metadata = has(ash::ext::hdr_metadata::NAME);
        if has_hdr_metadata {
            dev_exts.push(ash::ext::hdr_metadata::NAME.as_ptr());
        }

        // --- Vulkan Video decode (the FFmpeg-on-our-device path) ---------------------
        // Probed, never required: a capable stack gets the video extensions, a second
        // (decode) queue, and the features FFmpeg's decoder needs; anything less means
        // `vulkan_decode() == None` and the decoder chain falls back (VAAPI/software).
        // SAFETY: per the Vulkan contract above - a read-only query on the live instance/device,
        // filling locals returned by value.
        let dev_props = unsafe { instance.get_physical_device_properties(pdev) };
        let dev_is_13 = vk::api_version_major(dev_props.api_version) > 1
            || vk::api_version_minor(dev_props.api_version) >= 3;
        let mut have_pid = vk::PhysicalDevicePresentIdFeaturesKHR::default();
        let mut have_pwait = vk::PhysicalDevicePresentWaitFeaturesKHR::default();
        let mut have_f11 = vk::PhysicalDeviceVulkan11Features::default();
        let mut have_f12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut have_f13 = vk::PhysicalDeviceVulkan13Features::default();
        // Present-id/present-wait (on-glass timing, latency plan T0.2): query the feature
        // structs only when the device lists both extensions.
        let present_wait_exts =
            has(ash::khr::present_id::NAME) && has(ash::khr::present_wait::NAME);
        let mut have_f2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut have_f11)
            .push_next(&mut have_f12)
            .push_next(&mut have_f13);
        if present_wait_exts {
            have_f2 = have_f2.push_next(&mut have_pid).push_next(&mut have_pwait);
        }
        // SAFETY: per the Vulkan contract above - a read-only query on the live instance/device,
        // filling locals returned by value.
        unsafe { instance.get_physical_device_features2(pdev, &mut have_f2) };
        // Copy the one base-features fact out NOW: `have_f2` mutably borrows the chained
        // structs through its pNext chain, so any later use of it would pin those borrows —
        // every read of a chained struct below must come after this, have_f2's last use.
        let have_shader_int16 = have_f2.features.shader_int16;
        let present_wait_ok = present_wait_exts
            && have_pid.present_id == vk::TRUE
            && have_pwait.present_wait == vk::TRUE;
        let features_ok = have_f11.sampler_ycbcr_conversion == vk::TRUE
            && have_f12.timeline_semaphore == vk::TRUE
            && have_f13.synchronization2 == vk::TRUE;
        // PyroWave decode (the wired-LAN wavelet codec, design/pyrowave-codec-plan.md §4.5):
        // plain Vulkan-1.3 compute on THIS device — no video extensions. Probed alongside so a
        // capable device gets the features enabled below and advertises the codec; anything
        // less simply never sets the CODEC_PYROWAVE bit.
        let pyrowave_ok = dev_is_13
            && have_shader_int16 == vk::TRUE
            && have_f12.storage_buffer8_bit_access == vk::TRUE
            && have_f12.timeline_semaphore == vk::TRUE
            && have_f13.subgroup_size_control == vk::TRUE
            && have_f13.compute_full_subgroups == vk::TRUE
            && have_f13.synchronization2 == vk::TRUE;

        // The decode queue family + which codec operations it can run.
        let decode_family: Option<(u32, vk::VideoCodecOperationFlagsKHR)> = {
            // SAFETY: per the Vulkan contract above - a read-only query on the live
            // instance/device, filling locals returned by value.
            let n = unsafe { instance.get_physical_device_queue_family_properties2_len(pdev) };
            let mut video: Vec<vk::QueueFamilyVideoPropertiesKHR> =
                vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
            let mut props: Vec<vk::QueueFamilyProperties2> = video
                .iter_mut()
                .map(|v| vk::QueueFamilyProperties2::default().push_next(v))
                .collect();
            // SAFETY: per the Vulkan contract above - a read-only query on the live
            // instance/device, filling locals returned by value.
            unsafe { instance.get_physical_device_queue_family_properties2(pdev, &mut props) };
            // `props` mutably borrows `video` (push_next); copy the flags out, then
            // read the driver-filled video properties directly.
            let flags: Vec<vk::QueueFlags> = props
                .iter()
                .map(|p| p.queue_family_properties.queue_flags)
                .collect();
            drop(props);
            flags
                .iter()
                .zip(&video)
                .enumerate()
                .find(|(_, (f, _))| f.contains(vk::QueueFlags::VIDEO_DECODE_KHR))
                .map(|(i, (_, v))| (i as u32, v.video_codec_operations))
        };

        const VIDEO_BASE: [&std::ffi::CStr; 2] = [
            ash::khr::video_queue::NAME,
            ash::khr::video_decode_queue::NAME,
        ];
        const VIDEO_CODECS: [&std::ffi::CStr; 3] = [
            ash::khr::video_decode_h264::NAME,
            ash::khr::video_decode_h265::NAME,
            c"VK_KHR_video_decode_av1",
        ];
        let codec_exts: Vec<&std::ffi::CStr> =
            VIDEO_CODECS.into_iter().filter(|n| has(n)).collect();
        let video_ok = dev_is_13
            && features_ok
            && decode_family.is_some()
            && VIDEO_BASE.iter().all(|n| has(n))
            && !codec_exts.is_empty();

        let (decode_qf, decode_caps) = decode_family.unwrap_or((qfi, Default::default()));
        let mut video_ext_names: Vec<&std::ffi::CStr> = Vec::new();
        if video_ok {
            video_ext_names.extend(VIDEO_BASE);
            video_ext_names.extend(&codec_exts);
            // Optional decoder niceties FFmpeg uses when present.
            for opt in [c"VK_KHR_video_maintenance1", c"VK_KHR_video_maintenance2"] {
                if has(opt) {
                    video_ext_names.push(opt);
                }
            }
            dev_exts.extend(video_ext_names.iter().map(|n| n.as_ptr()));
            tracing::info!(
                decode_qf,
                caps = ?decode_caps,
                exts = ?video_ext_names,
                "Vulkan Video decode available on this device"
            );
        } else {
            tracing::info!(
                dev_is_13,
                features_ok,
                decode_family = decode_family.is_some(),
                "Vulkan Video decode unavailable — decoder falls back (VAAPI/software)"
            );
        }

        // Present-id/present-wait: enable when fully supported — the presenter then runs
        // the on-glass PresentTimer; otherwise the display stamp stays submit-time.
        if present_wait_ok {
            dev_exts.push(ash::khr::present_id::NAME.as_ptr());
            dev_exts.push(ash::khr::present_wait::NAME.as_ptr());
        }
        let mut en_pid = vk::PhysicalDevicePresentIdFeaturesKHR::default().present_id(true);
        let mut en_pwait = vk::PhysicalDevicePresentWaitFeaturesKHR::default().present_wait(true);

        // Enable only the features the video path needs, and only where supported
        // (harmless when the path is off; reported to FFmpeg via device_features).
        let mut en_f11 = vk::PhysicalDeviceVulkan11Features::default()
            .sampler_ycbcr_conversion(have_f11.sampler_ycbcr_conversion == vk::TRUE);
        let mut en_f12 = vk::PhysicalDeviceVulkan12Features::default()
            .timeline_semaphore(have_f12.timeline_semaphore == vk::TRUE)
            .storage_buffer8_bit_access(pyrowave_ok)
            .shader_float16(pyrowave_ok && have_f12.shader_float16 == vk::TRUE);
        let mut en_f13 = vk::PhysicalDeviceVulkan13Features::default()
            .synchronization2(have_f13.synchronization2 == vk::TRUE)
            .subgroup_size_control(pyrowave_ok)
            .compute_full_subgroups(pyrowave_ok);
        let mut en_f2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut en_f11)
            .push_next(&mut en_f12)
            .push_next(&mut en_f13);
        if present_wait_ok {
            en_f2 = en_f2.push_next(&mut en_pid).push_next(&mut en_pwait);
        }
        en_f2.features.shader_int16 = if pyrowave_ok { vk::TRUE } else { vk::FALSE };

        let priorities = [1.0f32];
        let mut queue_info = vec![vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qfi)
            .queue_priorities(&priorities)];
        if video_ok && decode_qf != qfi {
            queue_info.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(decode_qf)
                    .queue_priorities(&priorities),
            );
        }
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let device = unsafe {
            instance.create_device(
                pdev,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_info)
                    .enabled_extension_names(&dev_exts)
                    .push_next(&mut en_f2),
                None,
            )
        }
        .context("vkCreateDevice")?;
        let swap_d = ash::khr::swapchain::Device::new(&instance, &device);
        let present_timer = present_wait_ok.then(|| {
            super::present_timing::PresentTimer::spawn(ash::khr::present_wait::Device::new(
                &instance, &device,
            ))
        });
        tracing::info!(
            present_wait = present_wait_ok,
            "on-glass present timing (VK_KHR_present_wait)"
        );
        let hdr_metadata_d =
            has_hdr_metadata.then(|| ash::ext::hdr_metadata::Device::new(&instance, &device));
        // SAFETY: per the Vulkan contract above - a read-only query on the live instance/device,
        // filling locals returned by value.
        let queue = unsafe { device.get_device_queue(qfi, 0) };
        #[cfg(target_os = "linux")]
        let hw = if hw_capable {
            Some(HwCtx {
                ext_mem_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
            })
        } else {
            None
        };
        #[cfg(windows)]
        let hw_win = win_capable.then(|| HwCtxWin {
            ext_mem_win32: ash::khr::external_memory_win32::Device::new(&instance, &device),
        });
        let csc = CscPass::new(&device, vk::Format::R8G8B8A8_UNORM)?;
        // Starts SDR like `csc`; an HDR (PQ) pyrowave session rebuilds it at the 10-bit
        // intermediate via `set_hdr_mode`, exactly like the H.26x pass.
        #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
        let csc_planar = if pyrowave_ok {
            Some(CscPass::new_planar(&device, vk::Format::R8G8B8A8_UNORM)?)
        } else {
            None
        };

        // The exported handle bundle: FFmpeg Vulkan Video handles when the device can
        // decode, AND (Windows) the D3D11-interop facts — so it's built whenever EITHER
        // consumer needs it; `video_decode`/`d3d11_import` tell the decoder chain which
        // paths are real. Extension lists must mirror creation exactly — FFmpeg keys its
        // code paths off the strings.
        // One lock per device for queue external sync (FFmpeg + Skia + this presenter
        // all funnel their queue calls through it — see the `queue_lock` field docs).
        let queue_lock = std::sync::Arc::new(pf_client_core::video::QueueLock::new());
        #[cfg(windows)]
        let export_worthy = video_ok || win_capable || pyrowave_ok;
        #[cfg(not(windows))]
        let export_worthy = video_ok || pyrowave_ok;
        let video_export = if export_worthy {
            // SAFETY: per the Vulkan contract above - a read-only query on the live
            // instance/device, filling locals returned by value.
            let qf_props = unsafe { instance.get_physical_device_queue_family_properties(pdev) };
            let mut device_extensions: Vec<CString> =
                vec![CString::from(ash::khr::swapchain::NAME)];
            #[cfg(target_os = "linux")]
            if hw_capable {
                device_extensions
                    .extend(dmabuf::DEVICE_EXTENSIONS.iter().map(|n| CString::from(*n)));
            }
            #[cfg(windows)]
            if win_capable {
                device_extensions.extend(
                    crate::d3d11::DEVICE_EXTENSIONS
                        .iter()
                        .map(|n| CString::from(*n)),
                );
            }
            if has_hdr_metadata {
                device_extensions.push(CString::from(ash::ext::hdr_metadata::NAME));
            }
            device_extensions.extend(video_ext_names.iter().map(|n| CString::from(*n)));
            Some(pf_client_core::video::VulkanDecodeDevice {
                get_instance_proc_addr: entry.static_fn().get_instance_proc_addr as usize,
                instance: instance.handle().as_raw() as usize,
                physical_device: pdev.as_raw() as usize,
                device: device.handle().as_raw() as usize,
                vendor_id: dev_props.vendor_id,
                device_name: dev_props
                    .device_name_as_c_str()
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                graphics_qf: qfi,
                graphics_queue_flags: qf_props[qfi as usize].queue_flags.as_raw(),
                decode_qf,
                decode_video_caps: decode_caps.as_raw(),
                instance_extensions: instance_extensions
                    .iter()
                    .map(|e| CString::new(e.as_str()).unwrap())
                    .collect(),
                device_extensions,
                f_sampler_ycbcr: have_f11.sampler_ycbcr_conversion == vk::TRUE,
                f_timeline_semaphore: have_f12.timeline_semaphore == vk::TRUE,
                f_synchronization2: have_f13.synchronization2 == vk::TRUE,
                f_shader_int16: pyrowave_ok,
                f_storage_buffer8: pyrowave_ok,
                f_subgroup_size_control: pyrowave_ok,
                f_compute_full_subgroups: pyrowave_ok,
                f_shader_float16: pyrowave_ok && have_f12.shader_float16 == vk::TRUE,
                api_version: dev_props.api_version,
                queue_families: queue_info.iter().map(|q| q.queue_family_index).collect(),
                pyrowave_decode: pyrowave_ok,
                video_decode: video_ok,
                // The phase-lock gate: real on-glass latch stamps exist only when the
                // present-wait timer runs (see `PresentTimer`).
                present_timing: present_timer.is_some(),
                #[cfg(windows)]
                d3d11_import: win_capable,
                #[cfg(not(windows))]
                d3d11_import: false,
                // Filled in below — the HDR10 surface facts arrive with pick_formats.
                d3d11_hdr10: false,
                adapter_luid,
                queue_lock: queue_lock.clone(),
            })
        } else {
            None
        };
        #[cfg(windows)]
        let mut video_export = video_export;

        let (format, hdr10_format) = pick_formats(&surface_i, pdev, surface, has_colorspace_ext)?;
        // The D3D11VA backend may emit its HDR (RGB10 PQ) ring only when this device can
        // import the 10-bit texture AND the surface offers an HDR10 swapchain to pass it
        // through to; otherwise a PQ stream keeps the decoder-side tonemap to sRGB.
        #[cfg(windows)]
        if let Some(v) = video_export.as_mut() {
            v.d3d11_hdr10 = win_capable && import_rgb10 && hdr10_format.is_some();
        }
        let mut pref = pref;
        pref.vrr_fifo_opt_in = vrr_fifo_opt_in();
        let present_mode = pick_present_mode(&surface_i, pdev, surface, pref)?;
        tracing::info!(
            ?format,
            ?hdr10_format,
            ?present_mode,
            vsync = pref.vsync,
            allow_vrr = pref.allow_vrr,
            hdr_metadata = has_hdr_metadata,
            "swapchain config"
        );
        let overlay_pipe = OverlayPipe::new(&device, format.format)?;

        // SAFETY: per the Vulkan contract above - recorded into a command buffer this code owns
        // and has begun, referencing handles it also owns; nothing is submitted until the
        // recording is ended.
        let cmd_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(qfi),
                None,
            )
        }?;
        // SAFETY: per the Vulkan contract above - recorded into a command buffer this code owns
        // and has begun, referencing handles it also owns; nothing is submitted until the
        // recording is ended.
        let cmd_buf = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }?[0];
        let acquire_sem =
            // SAFETY: per the Vulkan contract above - a create/allocate call on the live device,
            // over builder structs that are locals outliving the call; the handle it returns is
            // owned by the value being built here.
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?;
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let fence = unsafe {
            device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )
        }?;

        let mut p = Presenter {
            entry,
            instance,
            surface_i,
            surface,
            pdev,
            mem_props,
            device,
            swap_d,
            queue,
            qfi,
            #[cfg(target_os = "linux")]
            hw,
            #[cfg(windows)]
            hw_win,
            csc,
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            csc_planar,
            video_export,
            overlay_pipe,
            retired_hw: None,
            queue_lock,
            format,
            hdr10_format,
            hdr_active: false,
            hdr_downgrade_warned: false,
            hdr_metadata_d,
            hdr_meta: None,
            video_format: vk::Format::R8G8B8A8_UNORM,
            present_mode,
            swapchain: vk::SwapchainKHR::null(),
            images: Vec::new(),
            extent: vk::Extent2D::default(),
            render_sems: Vec::new(),
            acquire_sem,
            fence,
            cmd_pool,
            cmd_buf,
            staging: None,
            video: None,
            submitted: false,
            present_timer,
            next_present_id: 0,
            last_presented: None,
        };
        p.recreate_swapchain(window)?;
        Ok(p)
    }
}

/// The physical devices' marketing names — the shells' GPU-picker source
/// (`punktfunk-session --list-adapters`). No surface and no logical device; discrete
/// GPUs first (mirroring `pick_device`'s tie-break), duplicates collapsed (the name is
/// the whole `PUNKTFUNK_VK_ADAPTER` match key, so a second identical card adds nothing).
/// Same 1.3 instance the presenter creates, so the list matches what streaming sees.
pub fn list_adapters() -> Result<Vec<String>> {
    // SAFETY: per the Vulkan contract above - a create/allocate call on the live device, over
    // builder structs that are locals outliving the call; the handle it returns is owned by the
    // value being built here.
    let entry = unsafe { ash::Entry::load() }.context("libvulkan not loadable")?;
    let app_name = CString::new("punktfunk-session").unwrap();
    let app_info = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .api_version(vk::API_VERSION_1_3);
    // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this type
    // and live for the call, and every builder struct is a local that outlives it.
    let instance = unsafe {
        entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app_info),
            None,
        )
    }
    .context("vkCreateInstance")?;
    // SAFETY: per the Vulkan contract above - a read-only query on the live instance/device,
    // filling locals returned by value.
    let mut ranked: Vec<(u8, String)> = unsafe { instance.enumerate_physical_devices() }?
        .into_iter()
        .map(|d| {
            // SAFETY: per the Vulkan contract above - a read-only query on the live
            // instance/device, filling locals returned by value.
            let props = unsafe { instance.get_physical_device_properties(d) };
            let rank = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 0u8,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
                _ => 2,
            };
            let name = props
                .device_name_as_c_str()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default();
            (rank, name)
        })
        .filter(|(_, n)| !n.is_empty())
        .collect();
    // SAFETY: per the Vulkan contract above - this destroys objects this type owns, and the GPU is
    // known idle for them (the fence/queue-wait on the path here, or the swapchain being retired),
    // which is the obligation that makes a destroy sound rather than the handle merely being non-
    // null.
    unsafe { instance.destroy_instance(None) };
    ranked.sort_by_key(|(r, _)| *r); // stable: enumeration order within each tier
    let mut names: Vec<String> = Vec::new();
    for (_, n) in ranked {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    Ok(names)
}

/// First physical device with a queue family that does graphics + present here;
/// `PUNKTFUNK_VK_DEVICE=<index>` overrides on multi-GPU boxes.
fn pick_device(
    instance: &ash::Instance,
    surface_i: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> Result<(vk::PhysicalDevice, u32)> {
    // SAFETY: per the Vulkan contract above - a read-only query on the live instance/device,
    // filling locals returned by value.
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    let forced: Option<usize> = std::env::var("PUNKTFUNK_VK_DEVICE")
        .ok()
        .and_then(|v| v.parse().ok());
    let mut candidates: Vec<vk::PhysicalDevice> = match forced {
        Some(i) => devices.get(i).copied().into_iter().collect(),
        None => devices,
    };
    // Rank the candidates (stable sort; the index override wins outright):
    // 1. The Settings GPU pick — `PUNKTFUNK_VK_ADAPTER` carries the adapter's marketing
    //    name (the WinUI shell's picker stores DXGI's, which matches Vulkan's for the
    //    same GPU): exact match, then substring, plain order when nothing matches
    //    (eGPU unplugged, stale setting).
    // 2. Discrete over integrated: enumeration order puts the iGPU FIRST on some
    //    hybrids (observed: Ryzen iGPU ahead of an RTX dGPU), and the iGPU's video
    //    engine is the far weaker decoder — first-enumerated was a silent footgun.
    if forced.is_none() {
        let want = std::env::var("PUNKTFUNK_VK_ADAPTER")
            .ok()
            .map(|w| w.trim().to_lowercase())
            .filter(|w| !w.is_empty());
        candidates.sort_by_key(|d| {
            // SAFETY: per the Vulkan contract above - a read-only query on the live
            // instance/device, filling locals returned by value.
            let props = unsafe { instance.get_physical_device_properties(*d) };
            let name = props
                .device_name_as_c_str()
                .map(|c| c.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let name_rank = match &want {
                Some(w) if name == *w => 0,
                Some(w) if name.contains(w.as_str()) || w.contains(&name) => 1,
                Some(_) => 2,
                None => 0,
            };
            let type_rank = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 0,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
                _ => 2,
            };
            (name_rank, type_rank)
        });
    }
    for pdev in candidates {
        // SAFETY: per the Vulkan contract above - a read-only query on the live instance/device,
        // filling locals returned by value.
        let families = unsafe { instance.get_physical_device_queue_family_properties(pdev) };
        for (i, f) in families.iter().enumerate() {
            let graphics = f.queue_flags.contains(vk::QueueFlags::GRAPHICS);
            let present =
                // SAFETY: per the Vulkan contract above - a read-only query on the live
                // instance/device, filling locals returned by value.
                unsafe { surface_i.get_physical_device_surface_support(pdev, i as u32, surface) }
                    .unwrap_or(false);
            if graphics && present {
                return Ok((pdev, i as u32));
            }
        }
    }
    bail!("no Vulkan device with a graphics+present queue family")
}

/// SDR: prefer BGRA8 UNORM (the near-universal presentable format); RGBA8 second; else
/// whatever the surface offers first. UNORM (not SRGB) — the decoded RGBA is already
/// display-referred, the blit must not re-encode it. HDR: a 10-bit UNORM format paired
/// with the HDR10/ST.2084 colorspace, when the instance ext + surface offer one (KDE/
/// gamescope with HDR enabled; absent elsewhere → the shader tonemaps instead).
pub(super) fn pick_formats(
    surface_i: &ash::khr::surface::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    colorspace_ext: bool,
) -> Result<(vk::SurfaceFormatKHR, Option<vk::SurfaceFormatKHR>)> {
    // `PUNKTFUNK_HDR10=0` (explicit-off grammar) refuses the HDR10/ST.2084 swapchain outright,
    // pinning PQ streams to the shader tonemap on an SDR surface. Two reasons this exists:
    // desktop compositors newly offer HDR10 even on SDR desktops (GNOME 48 / Plasma 6 with
    // Mesa ≥ 25.1 — a lane that otherwise engages silently), and it is the A/B lever that
    // splits "HDR10 passthrough composes wrong" from "the decoded planes are wrong" in the
    // field without rebuilding anything.
    let colorspace_ext = colorspace_ext
        && !std::env::var("PUNKTFUNK_HDR10")
            .is_ok_and(|v| matches!(v.as_str(), "0" | "false" | "off" | "no"));
    // SAFETY: per the Vulkan contract above - a read-only query on the live instance/device,
    // filling locals returned by value.
    let formats = unsafe { surface_i.get_physical_device_surface_formats(pdev, surface) }?;
    let mut sdr = None;
    for want in [vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM] {
        if let Some(f) = formats
            .iter()
            .find(|f| f.format == want && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
        {
            sdr = Some(*f);
            break;
        }
    }
    let sdr = sdr
        .or_else(|| formats.first().copied())
        .ok_or_else(|| anyhow!("surface offers no formats"))?;
    let hdr10 = colorspace_ext
        .then(|| {
            formats
                .iter()
                .find(|f| {
                    f.color_space == vk::ColorSpaceKHR::HDR10_ST2084_EXT
                        && matches!(
                            f.format,
                            vk::Format::A2B10G10R10_UNORM_PACK32
                                | vk::Format::A2R10G10B10_UNORM_PACK32
                        )
                })
                .copied()
        })
        .flatten();
    Ok((sdr, hdr10))
}

/// What the user asked the presentation to be, resolved into a swapchain present mode by
/// [`present_mode_chain`] (design/desktop-presentation-rebuild.md WP3).
#[derive(Clone, Copy, Debug, Default)]
pub struct PresentPref {
    /// Tear-free presentation (the `vsync` setting, default on).
    pub vsync: bool,
    /// Let a variable-refresh display follow the stream cadence (`allow_vrr`, default on).
    pub allow_vrr: bool,
    /// Opt-in for the VRR FIFO-first ladder (`PUNKTFUNK_VRR_FIFO=1`). Off by default on
    /// measured evidence — see [`present_mode_chain`].
    pub vrr_fifo_opt_in: bool,
    /// The session STARTED fullscreen. The mode is chosen once, at swapchain creation, so
    /// this is the starting state and an F11 mid-session does not re-pick — consistent
    /// with the shells' "Display changes apply from the next session" footer, and why
    /// live present-mode switching is an explicit non-goal.
    pub fullscreen: bool,
}

/// The preference ladder, most to least wanted. The caller takes the first entry the
/// surface actually offers; FIFO ends every chain because the spec guarantees it.
///
/// * **V-Sync off** — IMMEDIATE (tears, no wait at all), then FIFO_RELAXED (tears only on
///   a late frame), then the tear-free modes. Asking for tearing and silently getting
///   vsync is a lie the stats line now exposes, but the ladder still degrades safely.
/// * **V-Sync on + VRR allowed + fullscreen + `PUNKTFUNK_VRR_FIFO=1`** — FIFO first. On a
///   variable-refresh panel with direct scanout the FIFO present IS the flip, so the panel
///   follows the stream's cadence; MAILBOX would decouple presents from scanout and
///   re-quantize to the compositor's clock.
///
///   ⚠ **Opt-in, not default, on measured evidence.** It was default-on until an on-glass
///   A/B (.21, GNOME/Wayland, NVIDIA, *non*-VRR 60 Hz panel, 2026-08-02) showed this
///   route costs ~27 ms of display stage versus MAILBOX on the same box, reproducibly:
///   `display 28.4 ms (pace 11.8 + latch 16.6)` on FIFO against `1.4 ms (0.2 + 1.2)` on
///   MAILBOX. Under a compositor the FIFO present's on-glass confirmation arrives a whole
///   refresh later, and the presenter serialises behind it. The upside on a genuine VRR
///   panel is real but UNMEASURED — no VRR display was available — and a default that is
///   measurably worse on the hardware we could test, in exchange for an unproven win on
///   hardware we could not, is the wrong way round. Flip the default once a VRR panel
///   confirms the win (WP6 open item).
/// * **Otherwise** — MAILBOX, then FIFO: the shipped default. MAILBOX never queues more
///   than the newest frame, so an arrival-paced presenter doesn't block in the present
///   queue (a measured 11-13 ms standing wait at 60 Hz when the compositor holds images
///   for a vblank pass, or when arrival cadence drifts against refresh).
///
/// AMD's Windows driver offers no MAILBOX (NVIDIA does), so those clients land on FIFO —
/// expected, not a misconfiguration, and now visible in the `present:` stats line.
fn present_mode_chain(pref: PresentPref) -> [vk::PresentModeKHR; 4] {
    use vk::PresentModeKHR as M;
    if !pref.vsync {
        [M::IMMEDIATE, M::FIFO_RELAXED, M::MAILBOX, M::FIFO]
    } else if pref.allow_vrr && pref.fullscreen && pref.vrr_fifo_opt_in {
        [M::FIFO, M::MAILBOX, M::FIFO_RELAXED, M::IMMEDIATE]
    } else {
        [M::MAILBOX, M::FIFO, M::FIFO_RELAXED, M::IMMEDIATE]
    }
}

/// `PUNKTFUNK_VRR_FIFO=1` — opt into the FIFO-first ladder for variable-refresh panels.
/// See [`present_mode_chain`] for the measurement that made this opt-in rather than
/// default.
fn vrr_fifo_opt_in() -> bool {
    std::env::var("PUNKTFUNK_VRR_FIFO").is_ok_and(|v| v != "0")
}

/// Resolve the present mode: `PUNKTFUNK_PRESENT_MODE` pins one outright (the debug lever,
/// unchanged), otherwise the first entry of [`present_mode_chain`] the surface offers.
fn pick_present_mode(
    surface_i: &ash::khr::surface::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    pref: PresentPref,
) -> Result<vk::PresentModeKHR> {
    // SAFETY: per the Vulkan contract above - a read-only query on the live instance/device,
    // filling locals returned by value.
    let modes = unsafe { surface_i.get_physical_device_surface_present_modes(pdev, surface) }?;
    let pinned = match std::env::var("PUNKTFUNK_PRESENT_MODE").ok().as_deref() {
        Some("fifo") => Some(vk::PresentModeKHR::FIFO),
        Some("immediate") => Some(vk::PresentModeKHR::IMMEDIATE),
        Some("fifo_relaxed") => Some(vk::PresentModeKHR::FIFO_RELAXED),
        Some("mailbox") => Some(vk::PresentModeKHR::MAILBOX),
        None => None,
        Some(other) => {
            tracing::warn!(
                value = other,
                "unknown PUNKTFUNK_PRESENT_MODE (expected fifo|mailbox|immediate|fifo_relaxed) — following the settings"
            );
            None
        }
    };
    if let Some(want) = pinned {
        if modes.contains(&want) {
            return Ok(want);
        }
        tracing::warn!(
            ?want,
            "PUNKTFUNK_PRESENT_MODE not offered by this surface — falling back"
        );
    }
    let chain = present_mode_chain(pref);
    let chosen = chain
        .iter()
        .copied()
        .find(|m| modes.contains(m))
        .unwrap_or(vk::PresentModeKHR::FIFO); // always available per spec
                                              // The one line that answers "did V-Sync off actually take?" — a request the surface
                                              // can't serve is a fact about the driver, and it must not look like our choice.
    if chosen != chain[0] {
        tracing::info!(
            requested = ?chain[0],
            active = ?chosen,
            vsync = pref.vsync,
            allow_vrr = pref.allow_vrr,
            "the surface does not offer the preferred present mode"
        );
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vk::PresentModeKHR as M;

    /// The preference ladders (WP3). Every chain must end at FIFO, which the spec
    /// guarantees exists — a chain whose entries a surface all refuses would otherwise
    /// have no landing.
    #[test]
    fn present_mode_chains_rank_by_intent() {
        let pref = |vsync, allow_vrr, fullscreen| PresentPref {
            vsync,
            allow_vrr,
            fullscreen,
            vrr_fifo_opt_in: true, // the ladder under test; the DEFAULT is off (see below)
        };

        // V-Sync off asks to tear, hardest first, and outranks the VRR rule (tearing
        // already gives a VRR-like latch, so the two never fight).
        assert_eq!(present_mode_chain(pref(false, true, true))[0], M::IMMEDIATE);
        assert_eq!(
            present_mode_chain(pref(false, false, false))[0],
            M::IMMEDIATE
        );
        assert_eq!(
            present_mode_chain(pref(false, true, true))[1],
            M::FIFO_RELAXED,
            "tears only on a late frame — the gentler tearing rung"
        );

        // Tear-free + VRR allowed + fullscreen prefers FIFO, so the flip IS the present
        // and a variable-refresh panel follows the stream — but ONLY when opted in.
        assert_eq!(present_mode_chain(pref(true, true, true))[0], M::FIFO);
        // Without the opt-in the shipped MAILBOX-first default stands: measured on glass
        // to be ~27 ms of display stage better on a non-VRR panel.
        assert_eq!(
            present_mode_chain(PresentPref {
                vsync: true,
                allow_vrr: true,
                fullscreen: true,
                vrr_fifo_opt_in: false,
            })[0],
            M::MAILBOX,
            "the VRR ladder is opt-in until a VRR panel confirms the win"
        );
        // Windowed, or VRR declined: the shipped MAILBOX-first default.
        assert_eq!(present_mode_chain(pref(true, true, false))[0], M::MAILBOX);
        assert_eq!(present_mode_chain(pref(true, false, true))[0], M::MAILBOX);
        assert_eq!(present_mode_chain(pref(true, false, false))[0], M::MAILBOX);

        // Every ladder can land: FIFO appears in all of them.
        for p in [
            pref(true, true, true),
            pref(true, true, false),
            pref(true, false, true),
            pref(false, true, true),
            pref(false, false, false),
        ] {
            assert!(
                present_mode_chain(p).contains(&M::FIFO),
                "FIFO is the guaranteed landing"
            );
        }
    }
}

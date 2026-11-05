//! Headless present loop that exercises the layer without an X server: a `VK_EXT_headless_surface`
//! swapchain, two frames in flight with per-frame fences and semaphores (no `queue_wait_idle`), and
//! a per-frame pattern (`cs2_vk_capture::testpattern`) that encodes its own frame number.
//!
//! Schedule mode (default, needs `CS2MCP_CAPTURE_DIR`): the harness plays the server. It writes
//! `request.json` at scheduled frames, waits for each `<id>.json`, and verifies the PNG pixel for
//! pixel against the pattern of the frame named by `presentIndex`. A swapchain recreate happens
//! mid-run, with captures before and after it.
//!
//! `--no-schedule` only presents (paced with `--pace-ms`), for external clients.
//!
//! Usage: `vk_present_harness [--frames N] [--width W] [--height H] [--pace-ms M] [--no-schedule]`.
//! Exit codes: 0 passed, 1 failed, 3 skipped (no headless surface, or no B8G8R8A8_UNORM).
use ash::{ext, khr, vk};
use cs2_vk_capture::testpattern;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Frame indices at which a capture is requested (in order; one in flight at a time).
const SCHEDULE: &[u32] = &[10, 11, 60, 100, 101, 170, 171, 200];
/// Swapchain is recreated at the start of this frame (after a device idle).
const RECREATE_AT: u32 = 150;
/// The swapchain is destroyed at the start of this frame with no device idle: the capture
/// requested at frame 200 may still be running on the GPU when its images go away.
const HARD_DESTROY_AT: u32 = 201;
const SLOTS: usize = 2;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

enum Outcome {
    Passed {
        presented: u32,
        captures: Vec<(String, u64)>,
    },
    Skipped(String),
}

struct Config {
    frames: u32,
    width: u32,
    height: u32,
    pace: Duration,
    schedule: bool,
    stop_file: Option<PathBuf>,
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        frames: 240,
        width: 640,
        height: 480,
        pace: Duration::ZERO,
        schedule: true,
        stop_file: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut num = |name: &str| -> Result<u64, String> {
            args.next()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| format!("{name} needs a number"))
        };
        match a.as_str() {
            "--frames" => cfg.frames = num("--frames")? as u32,
            "--width" => cfg.width = num("--width")? as u32,
            "--height" => cfg.height = num("--height")? as u32,
            "--pace-ms" => cfg.pace = Duration::from_millis(num("--pace-ms")?),
            "--no-schedule" => cfg.schedule = false,
            "--stop-file" => {
                cfg.stop_file = Some(PathBuf::from(
                    args.next().ok_or("--stop-file needs a path")?,
                ))
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if cfg.width < 256 || cfg.height < 16 {
        return Err("the pattern needs width >= 256 and height >= 16".into());
    }
    Ok(cfg)
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    match unsafe { run(&cfg) } {
        Ok(Outcome::Passed {
            presented,
            captures,
        }) => {
            for (id, pi) in &captures {
                println!("capture {id}: presentIndex {pi} pixel-exact");
            }
            println!(
                "presented {presented} frames at {}x{}",
                cfg.width, cfg.height
            );
            println!("captures verified: {}", captures.len());
            println!("PASS");
        }
        Ok(Outcome::Skipped(why)) => {
            println!("SKIP: {why}");
            std::process::exit(3);
        }
        Err(e) => {
            eprintln!("FAIL: {e}");
            std::process::exit(1);
        }
    }
}

fn check<T, E: std::fmt::Debug>(r: Result<T, E>, what: &str) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {e:?}"))
}

/// Everything about the swapchain that a recreate needs.
struct Chain {
    swapchain: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    render: Vec<vk::Semaphore>,
}

struct Ctx {
    device: ash::Device,
    swap_fn: khr::swapchain::Device,
    surface: vk::SurfaceKHR,
    format: vk::SurfaceFormatKHR,
    extent: vk::Extent2D,
    min_images: u32,
    composite: vk::CompositeAlphaFlagsKHR,
    usage: vk::ImageUsageFlags,
}

unsafe fn make_chain(ctx: &Ctx, old: vk::SwapchainKHR) -> Result<Chain, String> {
    let sc_ci = vk::SwapchainCreateInfoKHR::default()
        .surface(ctx.surface)
        .min_image_count(ctx.min_images)
        .image_format(ctx.format.format)
        .image_color_space(ctx.format.color_space)
        .image_extent(ctx.extent)
        .image_array_layers(1)
        .image_usage(ctx.usage)
        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
        .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
        .composite_alpha(ctx.composite)
        .present_mode(vk::PresentModeKHR::FIFO)
        .clipped(true)
        .old_swapchain(old);
    let swapchain = check(
        ctx.swap_fn.create_swapchain(&sc_ci, None),
        "create swapchain",
    )?;
    let images = check(
        ctx.swap_fn.get_swapchain_images(swapchain),
        "swapchain images",
    )?;
    let mut render = Vec::new();
    for _ in &images {
        render.push(check(
            ctx.device
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None),
            "render semaphore",
        )?);
    }
    Ok(Chain {
        swapchain,
        images,
        render,
    })
}

unsafe fn destroy_chain(ctx: &Ctx, chain: &Chain) {
    for &s in &chain.render {
        ctx.device.destroy_semaphore(s, None);
    }
    ctx.swap_fn.destroy_swapchain(chain.swapchain, None);
}

fn find_memory(
    mem: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..mem.memory_type_count).find(|&i| {
        bits & (1 << i) != 0 && mem.memory_types[i as usize].property_flags.contains(want)
    })
}

/// A host-visible, coherent buffer, mapped for the life of the run.
unsafe fn staging(
    device: &ash::Device,
    mem: &vk::PhysicalDeviceMemoryProperties,
    size: u64,
) -> Result<(vk::Buffer, vk::DeviceMemory, *mut u8), String> {
    let buffer = check(
        device.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC),
            None,
        ),
        "staging buffer",
    )?;
    let req = device.get_buffer_memory_requirements(buffer);
    let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let ty = find_memory(mem, req.memory_type_bits, want).ok_or("no host-visible memory")?;
    let memory = check(
        device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ty),
            None,
        ),
        "staging memory",
    )?;
    check(device.bind_buffer_memory(buffer, memory, 0), "bind staging")?;
    let ptr = check(
        device.map_memory(memory, 0, req.size, vk::MemoryMapFlags::empty()),
        "map staging",
    )? as *mut u8;
    Ok((buffer, memory, ptr))
}

/// Decode a PNG the layer wrote (RGB8) into packed RGB.
fn read_png(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .map_err(|e| format!("png header: {e}"))?;
    let size = reader.output_buffer_size().ok_or("png too large")?;
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png data: {e}"))?;
    if info.color_type != png::ColorType::Rgb || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "png is {:?}/{:?}, want RGB8",
            info.color_type, info.bit_depth
        ));
    }
    buf.truncate(info.buffer_size());
    Ok((info.width, info.height, buf))
}

/// Check a finished capture: layer metadata, then the pixels against the pattern.
fn verify_capture(dir: &Path, id: &str, w: u32, h: u32, asked_at: u32) -> Result<u64, String> {
    let json_bytes = std::fs::read(dir.join(format!("{id}.json"))).map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_slice(&json_bytes).map_err(|e| e.to_string())?;
    if let Some(err) = json.get("error") {
        return Err(format!("layer reported error for {id}: {err}"));
    }
    if json["pid"].as_u64() != Some(std::process::id() as u64) {
        return Err(format!("result pid {} is not this process", json["pid"]));
    }
    if json["exe"].as_str().unwrap_or("").is_empty() {
        return Err("result has no exe".into());
    }
    if json["width"].as_u64() != Some(w as u64) || json["height"].as_u64() != Some(h as u64) {
        return Err(format!(
            "result size {}x{}, want {w}x{h}",
            json["width"], json["height"]
        ));
    }
    let present_index = json["presentIndex"].as_u64().ok_or("no presentIndex")?;
    if present_index < asked_at as u64 {
        return Err(format!(
            "presentIndex {present_index} is before the request at frame {asked_at}"
        ));
    }
    let (pw, ph, rgb) = read_png(&dir.join(format!("{id}.png")))?;
    if (pw, ph) != (w, h) {
        return Err(format!("png is {pw}x{ph}"));
    }
    testpattern::verify_rgb(w, h, &rgb, present_index)?;
    Ok(present_index)
}

fn write_request(dir: &Path, id: &str) -> Result<(), String> {
    let tmp = dir.join(format!(".request.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{{\"id\":\"{id}\",\"format\":\"png\"}}"))
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join("request.json")).map_err(|e| e.to_string())
}

unsafe fn run(cfg: &Config) -> Result<Outcome, String> {
    let capture_dir: Option<PathBuf> = if cfg.schedule {
        Some(
            std::env::var_os("CS2MCP_CAPTURE_DIR")
                .map(PathBuf::from)
                .ok_or("schedule mode needs CS2MCP_CAPTURE_DIR")?,
        )
    } else {
        None
    };
    let entry = ash::Entry::load().map_err(|e| format!("load libvulkan: {e}"))?;
    let exts = check(
        entry.enumerate_instance_extension_properties(None),
        "instance extensions",
    )?;
    let has = |name: &CStr| {
        exts.iter()
            .any(|e| CStr::from_ptr(e.extension_name.as_ptr()) == name)
    };
    if !has(ext::headless_surface::NAME) {
        return Ok(Outcome::Skipped(
            "VK_EXT_headless_surface not exported".into(),
        ));
    }
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
    let inst_exts = [
        khr::surface::NAME.as_ptr(),
        ext::headless_surface::NAME.as_ptr(),
    ];
    let ci = vk::InstanceCreateInfo::default()
        .application_info(&app)
        .enabled_extension_names(&inst_exts);
    let instance = check(entry.create_instance(&ci, None), "create instance")?;

    let headless = ext::headless_surface::Instance::new(&entry, &instance);
    let surface_fn = khr::surface::Instance::new(&entry, &instance);
    let surface = check(
        headless.create_headless_surface(&vk::HeadlessSurfaceCreateInfoEXT::default(), None),
        "headless surface",
    )?;

    let pds = check(instance.enumerate_physical_devices(), "physical devices")?;
    let mut chosen = None;
    for pd in pds {
        for (fi, fam) in instance
            .get_physical_device_queue_family_properties(pd)
            .iter()
            .enumerate()
        {
            let fi = fi as u32;
            if fam.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                && surface_fn
                    .get_physical_device_surface_support(pd, fi, surface)
                    .unwrap_or(false)
            {
                chosen = Some((pd, fi));
                break;
            }
        }
        if chosen.is_some() {
            break;
        }
    }
    let Some((pd, family)) = chosen else {
        return Ok(Outcome::Skipped(
            "no queue family presents to the headless surface".into(),
        ));
    };
    let name = CStr::from_ptr(
        instance
            .get_physical_device_properties(pd)
            .device_name
            .as_ptr(),
    )
    .to_string_lossy()
    .into_owned();
    eprintln!("device: {name}");

    let caps = check(
        surface_fn.get_physical_device_surface_capabilities(pd, surface),
        "surface caps",
    )?;
    let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST;
    if !caps.supported_usage_flags.contains(usage) {
        return Ok(Outcome::Skipped(
            "surface lacks COLOR_ATTACHMENT|TRANSFER_DST".into(),
        ));
    }
    let formats = check(
        surface_fn.get_physical_device_surface_formats(pd, surface),
        "surface formats",
    )?;
    let Some(format) = formats
        .iter()
        .copied()
        .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
    else {
        return Ok(Outcome::Skipped("no B8G8R8A8_UNORM surface format".into()));
    };
    let extent = if caps.current_extent.width == u32::MAX {
        vk::Extent2D {
            width: cfg.width,
            height: cfg.height,
        }
    } else {
        caps.current_extent
    };
    let mut min_images = caps.min_image_count.max(2);
    if caps.max_image_count > 0 {
        min_images = min_images.min(caps.max_image_count);
    }
    let composite = if caps
        .supported_composite_alpha
        .contains(vk::CompositeAlphaFlagsKHR::OPAQUE)
    {
        vk::CompositeAlphaFlagsKHR::OPAQUE
    } else {
        vk::CompositeAlphaFlagsKHR::INHERIT
    };

    let prio = [1.0f32];
    let queue_ci = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(family)
        .queue_priorities(&prio)];
    let dev_exts = [khr::swapchain::NAME.as_ptr()];
    let dev_ci = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_ci)
        .enabled_extension_names(&dev_exts);
    let device = check(instance.create_device(pd, &dev_ci, None), "create device")?;
    let queue = device.get_device_queue(family, 0);
    let mem_props = instance.get_physical_device_memory_properties(pd);

    let ctx = Ctx {
        swap_fn: khr::swapchain::Device::new(&instance, &device),
        device,
        surface,
        format,
        extent,
        min_images,
        composite,
        usage,
    };
    let mut chain = make_chain(&ctx, vk::SwapchainKHR::null())?;

    let (w, h) = (extent.width, extent.height);
    let size = w as u64 * h as u64 * 4;
    let pool = check(
        ctx.device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        ),
        "command pool",
    )?;
    let cmds = check(
        ctx.device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(SLOTS as u32),
        ),
        "command buffers",
    )?;
    let mut bufs = Vec::new();
    for _ in 0..SLOTS {
        bufs.push(staging(&ctx.device, &mem_props, size)?);
    }
    let mut acquire = Vec::new();
    let mut fences = Vec::new();
    for _ in 0..SLOTS {
        acquire.push(check(
            ctx.device
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None),
            "acquire semaphore",
        )?);
        fences.push(check(
            ctx.device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            ),
            "fence",
        )?);
    }

    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let mut captures = Vec::new();
    let mut pending: Option<(String, u32, Instant)> = None;
    let mut next_capture = 0usize;
    let mut frame: u32 = 0;
    let mut recreated = false;
    // render semaphores of destroyed chains that must outlive them (see HARD_DESTROY_AT)
    let mut graveyard: Vec<vk::Semaphore> = Vec::new();
    let mut old_render: Vec<vk::Semaphore> = chain.render.clone();
    loop {
        // results of the capture in flight
        if let (Some((id, asked, since)), Some(dir)) = (pending.clone(), capture_dir.as_ref()) {
            let json = dir.join(format!("{id}.json"));
            if json.exists() {
                let pi = verify_capture(dir, &id, w, h, asked)?;
                captures.push((id, pi));
                pending = None;
            } else if since.elapsed() > CAPTURE_TIMEOUT {
                return Err(format!(
                    "capture {id} requested at frame {asked} never finished"
                ));
            }
        }
        let schedule_left = cfg.schedule && next_capture < SCHEDULE.len();
        if !cfg.schedule {
            // exit on the frame cap or when the stop file appears (lets validation flush)
            if frame >= cfg.frames || cfg.stop_file.as_ref().is_some_and(|p| p.exists()) {
                break;
            }
        } else if frame >= cfg.frames && pending.is_none() && !schedule_left {
            break;
        }
        if frame > cfg.frames + 2000 {
            return Err("captures did not finish in time".into());
        }
        if let (Some(dir), None, true) = (capture_dir.as_ref(), &pending, schedule_left) {
            if frame >= SCHEDULE[next_capture] {
                let id = format!("h{frame:04}-{next_capture}");
                write_request(dir, &id)?;
                pending = Some((id, frame, Instant::now()));
                next_capture += 1;
            }
        }

        if !recreated && frame == RECREATE_AT && cfg.schedule {
            // Drain, then rebuild the chain with the old one as `oldSwapchain`.
            check(ctx.device.device_wait_idle(), "idle before recreate")?;
            let old = chain.swapchain;
            let old_render_sems = chain.render.clone();
            chain = make_chain(&ctx, old)?;
            ctx.swap_fn.destroy_swapchain(old, None);
            for s in old_render_sems {
                ctx.device.destroy_semaphore(s, None);
            }
            old_render.clear();
            old_render = chain.render.clone();
            recreated = true;
            eprintln!("recreated swapchain at frame {frame}");
        }

        if frame == HARD_DESTROY_AT && cfg.schedule {
            // Only this process's own submits are waited for. The old chain's render semaphores
            // are kept alive: a present may still be waiting on them.
            check(
                ctx.device.wait_for_fences(&fences, true, u64::MAX),
                "wait own fences",
            )?;
            let old = chain.swapchain;
            let in_flight = pending.is_some();
            chain = make_chain(&ctx, old)?;
            ctx.swap_fn.destroy_swapchain(old, None);
            graveyard.append(&mut old_render);
            old_render = chain.render.clone();
            eprintln!("destroyed swapchain at frame {frame} (capture in flight: {in_flight})");
        }

        let slot = frame as usize % SLOTS;
        check(
            ctx.device.wait_for_fences(&[fences[slot]], true, u64::MAX),
            "wait slot",
        )?;
        check(ctx.device.reset_fences(&[fences[slot]]), "reset fence")?;
        let (idx, _) = check(
            ctx.swap_fn.acquire_next_image(
                chain.swapchain,
                u64::MAX,
                acquire[slot],
                vk::Fence::null(),
            ),
            "acquire",
        )?;
        let image = chain.images[idx as usize];

        // this frame's pattern into the slot's staging buffer
        let n = testpattern::pattern_number(frame as u64);
        let bgra = testpattern::expected_bgra(w, h, n);
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bufs[slot].2, bgra.len());

        let cmd = cmds[slot];
        check(
            ctx.device
                .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()),
            "reset cmd",
        )?;
        check(
            ctx.device
                .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default()),
            "begin cmd",
        )?;
        let barrier = |old, new, src, dst| {
            vk::ImageMemoryBarrier::default()
                .old_layout(old)
                .new_layout(new)
                .src_access_mask(src)
                .dst_access_mask(dst)
                .image(image)
                .subresource_range(range)
        };
        // ALL_COMMANDS: the acquire read the image in a stage the barrier must order after
        ctx.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier(
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
            )],
        );
        ctx.device.cmd_copy_buffer_to_image(
            cmd,
            bufs[slot].0,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                })],
        );
        ctx.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier(
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::PRESENT_SRC_KHR,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::empty(),
            )],
        );
        check(ctx.device.end_command_buffer(cmd), "end cmd")?;

        let waits = [acquire[slot]];
        let stages = [vk::PipelineStageFlags::TRANSFER];
        let cmd_list = [cmd];
        let signals = [chain.render[idx as usize]];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&waits)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cmd_list)
            .signal_semaphores(&signals);
        check(
            ctx.device.queue_submit(queue, &[submit], fences[slot]),
            "submit",
        )?;

        let present_waits = [chain.render[idx as usize]];
        let swapchains = [chain.swapchain];
        let indices = [idx];
        let present = vk::PresentInfoKHR::default()
            .wait_semaphores(&present_waits)
            .swapchains(&swapchains)
            .image_indices(&indices);
        check(
            ctx.swap_fn.queue_present(queue, &present),
            &format!("present frame {frame}"),
        )?;

        frame += 1;
        if !cfg.pace.is_zero() {
            std::thread::sleep(cfg.pace);
        }
    }

    check(ctx.device.device_wait_idle(), "final idle")?;
    for s in graveyard {
        ctx.device.destroy_semaphore(s, None);
    }
    for i in 0..SLOTS {
        ctx.device.destroy_fence(fences[i], None);
        ctx.device.destroy_semaphore(acquire[i], None);
        ctx.device.unmap_memory(bufs[i].1);
        ctx.device.destroy_buffer(bufs[i].0, None);
        ctx.device.free_memory(bufs[i].1, None);
    }
    ctx.device.destroy_command_pool(pool, None);
    destroy_chain(&ctx, &chain);
    ctx.device.destroy_device(None);
    surface_fn.destroy_surface(surface, None);
    instance.destroy_instance(None);
    Ok(Outcome::Passed {
        presented: frame,
        captures,
    })
}

//! The capture pipeline on the present path.
//!
//! Before the real present (`begin`): claim the single capture slot, take a pending
//! `request.json` for a capturable swapchain, record a copy of the image the present shows
//! (PRESENT_SRC -> TRANSFER_SRC, copy into a host-visible buffer, a buffer barrier to the host,
//! back to PRESENT_SRC) and submit it. The submit waits on the present's own wait semaphores and
//! signals a layer-owned semaphore; the real present is rewritten to wait on that semaphore
//! instead. Only the recording is guarded by `catch_unwind`; the submit is the last fallible step.
//! Once it succeeds the capture cannot fail back into the present.
//!
//! After the real present (`finish`): a background thread waits for the copy fence (once),
//! invalidates and reads the buffer, publishes the PNG and json, and returns the GPU objects. A
//! fence that does not signal in time is never waited on twice: its objects go to the device's
//! deferred list, which later presents poll with `vkGetFenceStatus`.
//!
//! The capture slot (`IN_FLIGHT`) is claimed with a compare-exchange at the top of `begin` and
//! released in exactly one place: `Pending::drop` (or `begin` when no capture was started).
use crate::convert;
use crate::dispatch::{check, need};
use crate::output::{self, FrameMeta};
use crate::registry::{self, DeviceData, Leftover, Readback, SwapchainRes, Target};
use ash::vk::{self, Handle};
use std::ffi::c_void;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Minimum spacing between `request.json` checks.
const POLL_INTERVAL: Duration = Duration::from_millis(16);
/// A request for a non-capturable target is held this long (or this many presents) before it is
/// answered with an error.
const MISS_WAIT: Duration = Duration::from_secs(2);
const MISS_PRESENTS: u64 = 60;
/// How long a swapchain's destruction waits for its in-flight copy.
const SWAPCHAIN_WAIT_NS: u64 = 2_000_000_000;

static LAST_CHECK: Mutex<Option<Instant>> = Mutex::new(None);
static PRESENTS: AtomicU64 = AtomicU64::new(0);
/// A capture owns the slot from `begin` until its `Pending` is dropped.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static JOBS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());
/// The request currently waiting for a capturable target, and since when.
static MISS: Mutex<Option<Miss>> = Mutex::new(None);

struct Miss {
    id: String,
    since: Instant,
    presents: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Fence wait for a copy. Default 10 s. `CS2MCP_CAPTURE_FENCE_TIMEOUT_MS` overrides it, but only in
/// debug builds or with the `test-knobs` feature. 0 means never wait: every capture then takes the
/// deferred path (a test knob).
fn fence_timeout_ns() -> u64 {
    static T: OnceLock<u64> = OnceLock::new();
    *T.get_or_init(|| {
        // The override is a test knob: honoured only in debug builds or with `test-knobs`.
        #[cfg(any(debug_assertions, feature = "test-knobs"))]
        if let Some(ms) = std::env::var("CS2MCP_CAPTURE_FENCE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            return ms.saturating_mul(1_000_000);
        }
        10_000_000_000
    })
}

/// `$CS2MCP_CAPTURE_DIR`, else `$XDG_RUNTIME_DIR/cs2mcp-capture`, else `/tmp/cs2mcp-capture-<uid>`
/// (Windows: else `%LOCALAPPDATA%\cs2mcp-capture`). The server creates the directory (0700); the
/// layer never creates it.
pub fn capture_dir() -> PathBuf {
    static DIR: LazyLock<PathBuf> = LazyLock::new(|| {
        if let Some(d) = std::env::var_os("CS2MCP_CAPTURE_DIR") {
            return PathBuf::from(d);
        }
        #[cfg(windows)]
        {
            let base = std::env::var_os("LOCALAPPDATA").unwrap_or_default();
            PathBuf::from(base).join("cs2mcp-capture")
        }
        #[cfg(unix)]
        {
            if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR") {
                return PathBuf::from(x).join("cs2mcp-capture");
            }
            // SAFETY: getuid cannot fail.
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/cs2mcp-capture-{uid}"))
        }
    });
    DIR.clone()
}

/// The capture directory must be a real directory (not a symlink), owned by the effective user,
/// with no group or other permission bits. Windows: a real directory (no reparse point) owned by
/// this process's user (or its token's default owner); its DACL is not checked (the default under
/// `%LOCALAPPDATA%` is private to the user).
fn dir_is_safe(dir: &Path) -> bool {
    let Ok(m) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    if !m.is_dir() || m.file_type().is_symlink() {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: geteuid cannot fail.
        let uid = unsafe { libc::geteuid() };
        m.uid() == uid && m.mode() & 0o077 == 0
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 && crate::winsec::owned_by_me(dir)
    }
}

/// At most one `request.json` check per [`POLL_INTERVAL`].
fn gate_open() -> bool {
    let mut last = lock(&LAST_CHECK);
    let now = Instant::now();
    if last.is_some_and(|prev| now.duration_since(prev) < POLL_INTERVAL) {
        return false;
    }
    *last = Some(now);
    true
}

/// A request to act on: `(id, format)`. An unreadable or invalid request is removed here.
fn take_request(dir: &Path) -> Option<(String, String)> {
    match output::read_request(dir) {
        Ok(Some(r)) => {
            if !output::valid_id(&r.id) {
                dlog!("dropping request with invalid id {:?}", r.id);
                output::consume(dir, &r.id);
                return None;
            }
            Some((r.id, r.format))
        }
        Ok(None) => None,
        Err(e) => {
            dlog!("dropping unreadable request: {e}");
            output::remove_request(dir);
            None
        }
    }
}

/// One capture between `begin` and its readback. Owns its command pool, fence and readback buffer
/// until they are released; the swapchain owns the cached readback and semaphore it borrows.
pub struct Pending {
    dev: Arc<DeviceData>,
    res: Arc<SwapchainRes>,
    dir: PathBuf,
    id: String,
    format: vk::Format,
    width: u32,
    height: u32,
    present_index: u64,
    started_ms: u64,
    image: u64,
    image_index: u32,
    pool: u64,
    cmd: u64,
    fence: u64,
    readback: Option<Readback>,
    sem: u64,
    /// The layer semaphore as the rewritten present's single wait. Boxed so the pointer handed to
    /// the driver stays valid when the `Pending` moves.
    wait_sem: Box<[vk::Semaphore; 1]>,
    /// The copy was submitted and its fence has not been released.
    submitted: bool,
    present_ok: bool,
    /// The result (or an error) has been published.
    done: bool,
    /// This capture owns the `IN_FLIGHT` slot; `Drop` releases it.
    claimed: bool,
}

impl Pending {
    /// The present to pass down: `orig` with its wait list replaced by the layer semaphore. The
    /// original waits were consumed by the copy submission.
    pub fn rewrite<'s>(&'s self, orig: &vk::PresentInfoKHR<'s>) -> vk::PresentInfoKHR<'s> {
        let mut out = *orig;
        out.wait_semaphore_count = 1;
        out.p_wait_semaphores = self.wait_sem.as_ptr();
        out
    }

    /// Hand the GPU objects back. `gpu_done` is false when the copy may still run: then the
    /// objects go to the device's deferred list instead of being freed.
    unsafe fn release(&mut self, gpu_done: bool, sem_reusable: bool) {
        let device = vk::Device::from_raw(self.dev.handle);
        let f = &self.dev.fns;
        let rb = self.readback.take();
        if gpu_done {
            unlist_fence(&self.res, self.fence);
            if let Some(r) = rb {
                park_readback(&self.res, r);
            }
            if self.fence != 0 {
                if let Some(d) = f.destroy_fence {
                    d(device, vk::Fence::from_raw(self.fence), null_mut());
                }
            }
            if self.pool != 0 {
                if let Some(d) = f.destroy_command_pool {
                    d(device, vk::CommandPool::from_raw(self.pool), null_mut());
                }
            }
        } else if self.pool != 0 || self.fence != 0 || rb.is_some() {
            // the swapchain keeps listing the fence until it is freed (see `free_leftover`)
            let left = Leftover {
                pool: self.pool,
                fence: self.fence,
                buffer: rb.map_or(0, |r| r.buffer),
                memory: rb.map_or(0, |r| r.memory),
                res: (self.fence != 0).then(|| self.res.clone()),
            };
            lock(&self.dev.deferred).push(left);
        }
        self.pool = 0;
        self.cmd = 0;
        self.fence = 0;
        self.submitted = false;
        if self.sem != 0 {
            park_semaphore(&self.res, self.image_index, self.sem, sem_reusable);
            self.sem = 0;
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        // SAFETY: the handles are this capture's own; `release` zeroes them, so each is freed once.
        unsafe {
            if !self.done && self.claimed {
                // no result was published (readback never started, or it panicked after the copy)
                output::publish_error(
                    &self.dir,
                    &self.id,
                    "capture abandoned before the result was published",
                );
            }
            if self.submitted {
                self.release(false, false);
            } else {
                self.release(true, true);
            }
        }
        if self.claimed {
            IN_FLIGHT.store(false, Ordering::Release);
        }
    }
}

/// Stop listing a fence on its swapchain (it has signalled, or it is being freed).
fn unlist_fence(res: &SwapchainRes, fence: u64) {
    if fence != 0 {
        lock(&res.state).inflight_fences.retain(|&f| f != fence);
    }
}

fn park_readback(res: &SwapchainRes, rb: Readback) {
    let mut st = lock(&res.state);
    if st.alive {
        st.readbacks.push(rb);
        return;
    }
    drop(st);
    // SAFETY: the swapchain is gone; nothing uses this buffer any more.
    unsafe { destroy_readback(&res.dev, rb) };
}

fn park_semaphore(res: &SwapchainRes, image: u32, sem: u64, reusable: bool) {
    let mut st = lock(&res.state);
    if reusable && st.alive && !st.semaphores.contains_key(&image) {
        st.semaphores.insert(image, sem);
        return;
    }
    drop(st);
    lock(&res.dev.retired).push(sem);
}

unsafe fn destroy_readback(dev: &DeviceData, rb: Readback) {
    let device = vk::Device::from_raw(dev.handle);
    if let Some(d) = dev.fns.destroy_buffer {
        d(device, vk::Buffer::from_raw(rb.buffer), null_mut());
    }
    if let Some(f) = dev.fns.free_memory {
        f(device, vk::DeviceMemory::from_raw(rb.memory), null_mut());
    }
}

/// Before the real present. `None` when nothing was captured. The request is then either answered
/// with `{id, error}` or left pending for a capturable target.
///
/// # Safety
/// `info` must be the present info the application passed to `vkQueuePresentKHR`.
pub unsafe fn begin(
    queue: vk::Queue,
    dev: &Arc<DeviceData>,
    family: Option<u32>,
    info: &vk::PresentInfoKHR,
) -> Option<Pending> {
    let present_index = PRESENTS.fetch_add(1, Ordering::Relaxed);
    if IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    // Everything up to the submit runs under catch_unwind. A panic here means "no capture": the
    // present goes through unchanged. The submit below is outside the guard on purpose.
    let staged = crate::caught(|| -> Result<Option<(String, Pending)>, String> {
        if !gate_open() {
            return Ok(None);
        }
        let dir = capture_dir();
        if std::fs::metadata(dir.join(output::REQUEST_FILE)).is_err() {
            return Ok(None);
        }
        if !dir_is_safe(&dir) {
            dlog!(
                "capture dir {} is not a private directory; ignoring requests",
                dir.display()
            );
            return Ok(None);
        }
        let Some((id, format)) = take_request(&dir) else {
            return Ok(None);
        };
        match pre_submit(dev, family, info, &dir, &id, &format, present_index) {
            Ok(Some(p)) => Ok(Some((id, p))),
            Ok(None) => Ok(None),
            Err(e) => {
                output::publish_error(&dir, &id, &e);
                Ok(None)
            }
        }
    });
    let (id, mut p) = match staged {
        Ok(Some(x)) => x,
        Ok(None) => {
            IN_FLIGHT.store(false, Ordering::Release);
            return None;
        }
        Err(e) => {
            dlog!("capture skipped: {e}");
            IN_FLIGHT.store(false, Ordering::Release);
            return None;
        }
    };
    p.claimed = true;
    // SAFETY: `p` owns the objects recorded above.
    match submit(&mut p, queue, info) {
        Ok(()) => {
            dlog!("capture {id}: copy of present {present_index} submitted");
            Some(p)
        }
        Err(e) => {
            p.done = true; // reported below; `p`'s drop must not report it again
            output::publish_error(&p.dir, &id, &e);
            None // `p` drops here: nothing was submitted, and the slot is released
        }
    }
}

/// Hold a request for a non-capturable target. Error only after [`MISS_WAIT`] or
/// [`MISS_PRESENTS`] presents with no capturable target for this request.
fn miss(id: &str, present_index: u64, reason: &str) -> Result<Option<Pending>, String> {
    let now = Instant::now();
    let mut m = lock(&MISS);
    if m.as_ref().is_none_or(|x| x.id != id) {
        *m = Some(Miss {
            id: id.to_owned(),
            since: now,
            presents: present_index,
        });
    }
    let x = m.as_ref().expect("set above");
    if now.duration_since(x.since) >= MISS_WAIT
        || present_index.saturating_sub(x.presents) >= MISS_PRESENTS
    {
        *m = None;
        return Err(reason.to_owned());
    }
    Ok(None)
}

/// Everything before the submit. `Ok(None)` means: keep the request pending.
unsafe fn pre_submit(
    dev: &Arc<DeviceData>,
    family: Option<u32>,
    info: &vk::PresentInfoKHR,
    dir: &Path,
    id: &str,
    format: &str,
    present_index: u64,
) -> Result<Option<Pending>, String> {
    if format != "png" {
        return Err(format!("unsupported format {format:?}"));
    }
    let Some(family) = family else {
        return miss(id, present_index, "queue is not tracked by the layer");
    };
    if !dev.queue_capturable(family) {
        return Err(format!(
            "queue family {family} has no graphics, compute or transfer capability"
        ));
    }
    let count = info.swapchain_count as usize;
    if count == 0 || info.p_swapchains.is_null() || info.p_image_indices.is_null() {
        return miss(id, present_index, "swapchain not capturable");
    }
    let swaps: Vec<u64> = std::slice::from_raw_parts(info.p_swapchains, count)
        .iter()
        .map(|s| s.as_raw())
        .collect();
    let indices = std::slice::from_raw_parts(info.p_image_indices, count);
    let Some(target) = registry::find_target(&swaps, indices) else {
        return miss(id, present_index, "swapchain not capturable");
    };
    *lock(&MISS) = None;
    if !convert::supported(target.format) {
        return Err(format!("unsupported swapchain format {:?}", target.format));
    }
    if target.color_space != vk::ColorSpaceKHR::SRGB_NONLINEAR {
        return Err(format!(
            "colour space {:?} is not converted to SDR; capture refused",
            target.color_space
        ));
    }
    prepare(dev, family, &target, dir, id, present_index).map(Some)
}

/// Create and record everything the copy needs. On error the partly built `Pending` is dropped
/// (nothing was submitted, so nothing is in flight).
unsafe fn prepare(
    dev: &Arc<DeviceData>,
    family: u32,
    t: &Target,
    dir: &Path,
    id: &str,
    present_index: u64,
) -> Result<Pending, String> {
    // Refuse rather than use a command buffer the loader cannot route (see `record`).
    let Some(set_loader) = dev.set_loader else {
        return Err(
            "loader data callback unavailable: refusing an unstamped command buffer".into(),
        );
    };
    let device = vk::Device::from_raw(dev.handle);
    let f = &dev.fns;
    let (w, h) = t.extent;
    let mut p = Pending {
        dev: dev.clone(),
        res: t.res.clone(),
        dir: dir.to_path_buf(),
        id: id.to_owned(),
        format: t.format,
        width: w,
        height: h,
        present_index,
        started_ms: now_ms(),
        image: t.image,
        image_index: t.index,
        pool: 0,
        cmd: 0,
        fence: 0,
        readback: None,
        sem: 0,
        wait_sem: Box::new([vk::Semaphore::null()]),
        submitted: false,
        present_ok: false,
        done: false,
        claimed: false,
    };

    // A transient pool per capture: no shared pool, so no external synchronisation to get wrong.
    let pool_ci = vk::CommandPoolCreateInfo {
        flags: vk::CommandPoolCreateFlags::TRANSIENT,
        queue_family_index: family,
        ..Default::default()
    };
    let create_pool = need(f.create_command_pool, "vkCreateCommandPool")?;
    let mut pool = vk::CommandPool::null();
    check(
        create_pool(device, &pool_ci, std::ptr::null(), &mut pool),
        "vkCreateCommandPool",
    )?;
    p.pool = pool.as_raw();

    let alloc = vk::CommandBufferAllocateInfo {
        command_pool: pool,
        level: vk::CommandBufferLevel::PRIMARY,
        command_buffer_count: 1,
        ..Default::default()
    };
    let allocate = need(f.allocate_command_buffers, "vkAllocateCommandBuffers")?;
    let mut cmd = vk::CommandBuffer::null();
    check(
        allocate(device, &alloc, &mut cmd),
        "vkAllocateCommandBuffers",
    )?;
    p.cmd = cmd.as_raw();
    // The ICD wrote its own magic into the handle. Layers below us (and the loader) route by the
    // first word, so it must be re-stamped through the loader's callback.
    let r = set_loader(device, cmd.as_raw() as usize as *mut c_void);
    if r != vk::Result::SUCCESS {
        return Err(format!("loader data callback failed: {r:?}"));
    }

    let create_fence = need(f.create_fence, "vkCreateFence")?;
    let mut fence = vk::Fence::null();
    check(
        create_fence(
            device,
            &vk::FenceCreateInfo::default(),
            std::ptr::null(),
            &mut fence,
        ),
        "vkCreateFence",
    )?;
    p.fence = fence.as_raw();

    let size = w as u64 * h as u64 * convert::SRC_BYTES_PER_PIXEL as u64;
    p.readback = Some(take_or_create_readback(dev, &t.res, size)?);
    p.sem = take_or_create_semaphore(dev, &t.res, t.index)?;
    p.wait_sem[0] = vk::Semaphore::from_raw(p.sem);

    record(&p)?;
    Ok(p)
}

fn take_or_create_readback(
    dev: &DeviceData,
    res: &SwapchainRes,
    size: u64,
) -> Result<Readback, String> {
    {
        let mut st = lock(&res.state);
        if let Some(pos) = st.readbacks.iter().position(|r| r.size == size) {
            return Ok(st.readbacks.swap_remove(pos));
        }
    }
    // SAFETY: plain allocation calls on the device this capture owns.
    unsafe { create_readback(dev, size) }
}

/// A host-visible buffer. Prefers HOST_CACHED (read back with an invalidate), then HOST_COHERENT.
unsafe fn create_readback(dev: &DeviceData, size: u64) -> Result<Readback, String> {
    let device = vk::Device::from_raw(dev.handle);
    let f = &dev.fns;
    let bi = vk::BufferCreateInfo {
        size,
        usage: vk::BufferUsageFlags::TRANSFER_DST,
        sharing_mode: vk::SharingMode::EXCLUSIVE,
        ..Default::default()
    };
    let create_buffer = need(f.create_buffer, "vkCreateBuffer")?;
    let mut buffer = vk::Buffer::null();
    check(
        create_buffer(device, &bi, std::ptr::null(), &mut buffer),
        "vkCreateBuffer",
    )?;
    let free_buffer = |b: vk::Buffer| {
        if let Some(d) = f.destroy_buffer {
            d(device, b, std::ptr::null());
        }
    };
    let mut reqs = vk::MemoryRequirements::default();
    let Ok(get_reqs) = need(
        f.get_buffer_memory_requirements,
        "vkGetBufferMemoryRequirements",
    ) else {
        free_buffer(buffer);
        return Err("vkGetBufferMemoryRequirements is not available".into());
    };
    get_reqs(device, buffer, &mut reqs);
    let want = [
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        vk::MemoryPropertyFlags::HOST_VISIBLE,
    ];
    let Some(mem_type) = want
        .iter()
        .find_map(|&flags| find_memory_type(&dev.mem, reqs.memory_type_bits, flags))
    else {
        free_buffer(buffer);
        return Err("no host-visible memory type for the readback buffer".into());
    };
    let ai = vk::MemoryAllocateInfo {
        allocation_size: reqs.size,
        memory_type_index: mem_type,
        ..Default::default()
    };
    let allocate = match need(f.allocate_memory, "vkAllocateMemory") {
        Ok(a) => a,
        Err(e) => {
            free_buffer(buffer);
            return Err(e);
        }
    };
    let mut memory = vk::DeviceMemory::null();
    if let Err(e) = check(
        allocate(device, &ai, std::ptr::null(), &mut memory),
        "vkAllocateMemory",
    ) {
        free_buffer(buffer);
        return Err(e);
    }
    let bind_ok = f
        .bind_buffer_memory
        .map(|b| b(device, buffer, memory, 0) == vk::Result::SUCCESS)
        .unwrap_or(false);
    if !bind_ok {
        if let Some(fr) = f.free_memory {
            fr(device, memory, std::ptr::null());
        }
        free_buffer(buffer);
        return Err("vkBindBufferMemory failed".into());
    }
    Ok(Readback {
        buffer: buffer.as_raw(),
        memory: memory.as_raw(),
        size,
    })
}

fn find_memory_type(
    mem: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> Option<u32> {
    let types = &mem.memory_types[..mem.memory_type_count as usize];
    types.iter().enumerate().find_map(|(i, t)| {
        (bits & (1 << i) != 0 && t.property_flags.contains(want)).then_some(i as u32)
    })
}

fn take_or_create_semaphore(
    dev: &DeviceData,
    res: &SwapchainRes,
    image: u32,
) -> Result<u64, String> {
    if let Some(s) = lock(&res.state).semaphores.remove(&image) {
        return Ok(s);
    }
    let create = need(dev.fns.create_semaphore, "vkCreateSemaphore")?;
    let mut sem = vk::Semaphore::null();
    // SAFETY: plain creation on the owned device.
    unsafe {
        check(
            create(
                vk::Device::from_raw(dev.handle),
                &vk::SemaphoreCreateInfo::default(),
                std::ptr::null(),
                &mut sem,
            ),
            "vkCreateSemaphore",
        )?;
    }
    Ok(sem.as_raw())
}

/// Record PRESENT_SRC -> TRANSFER_SRC, the copy, a buffer barrier to the host, and
/// TRANSFER_SRC -> PRESENT_SRC.
unsafe fn record(p: &Pending) -> Result<(), String> {
    let f = &p.dev.fns;
    let cmd = vk::CommandBuffer::from_raw(p.cmd);
    let rb = p.readback.ok_or("no readback buffer")?;
    let begin_info = vk::CommandBufferBeginInfo {
        flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT,
        ..Default::default()
    };
    let begin_cb = need(f.begin_command_buffer, "vkBeginCommandBuffer")?;
    check(begin_cb(cmd, &begin_info), "vkBeginCommandBuffer")?;
    let range = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    let image = vk::Image::from_raw(p.image);
    let barrier = |old, new, src_access, dst_access| vk::ImageMemoryBarrier {
        src_access_mask: src_access,
        dst_access_mask: dst_access,
        old_layout: old,
        new_layout: new,
        src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        image,
        subresource_range: range,
        ..Default::default()
    };
    let barrier_fn = need(f.cmd_pipeline_barrier, "vkCmdPipelineBarrier")?;
    let copy_fn = need(f.cmd_copy_image_to_buffer, "vkCmdCopyImageToBuffer")?;
    let to_src = [barrier(
        vk::ImageLayout::PRESENT_SRC_KHR,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
        vk::AccessFlags::TRANSFER_READ,
    )];
    barrier_fn(
        cmd,
        vk::PipelineStageFlags::ALL_COMMANDS,
        vk::PipelineStageFlags::TRANSFER,
        vk::DependencyFlags::empty(),
        0,
        std::ptr::null(),
        0,
        std::ptr::null(),
        1,
        to_src.as_ptr(),
    );
    let region = vk::BufferImageCopy {
        buffer_offset: 0,
        buffer_row_length: 0,
        buffer_image_height: 0,
        image_subresource: vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        },
        image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
        image_extent: vk::Extent3D {
            width: p.width,
            height: p.height,
            depth: 1,
        },
    };
    copy_fn(
        cmd,
        image,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::Buffer::from_raw(rb.buffer),
        1,
        &region,
    );
    // The fence orders the copy before the host, but does not make its writes visible to host
    // reads; a buffer barrier does (HOST_CACHED memory may be non-coherent).
    let to_host = [vk::BufferMemoryBarrier {
        src_access_mask: vk::AccessFlags::TRANSFER_WRITE,
        dst_access_mask: vk::AccessFlags::HOST_READ,
        src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        buffer: vk::Buffer::from_raw(rb.buffer),
        offset: 0,
        size: vk::WHOLE_SIZE,
        ..Default::default()
    }];
    barrier_fn(
        cmd,
        vk::PipelineStageFlags::TRANSFER,
        vk::PipelineStageFlags::HOST,
        vk::DependencyFlags::empty(),
        0,
        std::ptr::null(),
        1,
        to_host.as_ptr(),
        0,
        std::ptr::null(),
    );
    // back to PRESENT_SRC; dst access 0 (the present that follows is ordered by the semaphore)
    let back = [barrier(
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::ImageLayout::PRESENT_SRC_KHR,
        vk::AccessFlags::TRANSFER_READ,
        vk::AccessFlags::empty(),
    )];
    barrier_fn(
        cmd,
        vk::PipelineStageFlags::TRANSFER,
        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
        vk::DependencyFlags::empty(),
        0,
        std::ptr::null(),
        0,
        std::ptr::null(),
        1,
        back.as_ptr(),
    );
    let end_cb = need(f.end_command_buffer, "vkEndCommandBuffer")?;
    check(end_cb(cmd), "vkEndCommandBuffer")
}

/// Submit the copy. Last fallible step: on success the present is committed to the rewrite.
///
/// # Safety
/// `p` must be fully recorded; `info` is the present the application passed.
unsafe fn submit(
    p: &mut Pending,
    queue: vk::Queue,
    info: &vk::PresentInfoKHR,
) -> Result<(), String> {
    let f = &p.dev.fns;
    let waits: Vec<vk::Semaphore> = if info.wait_semaphore_count == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(info.p_wait_semaphores, info.wait_semaphore_count as usize)
            .to_vec()
    };
    let stages = vec![vk::PipelineStageFlags::TRANSFER; waits.len()];
    let signal = [vk::Semaphore::from_raw(p.sem)];
    let cmds = [vk::CommandBuffer::from_raw(p.cmd)];
    let submit = vk::SubmitInfo {
        wait_semaphore_count: waits.len() as u32,
        p_wait_semaphores: waits.as_ptr(),
        p_wait_dst_stage_mask: stages.as_ptr(),
        command_buffer_count: 1,
        p_command_buffers: cmds.as_ptr(),
        signal_semaphore_count: 1,
        p_signal_semaphores: signal.as_ptr(),
        ..Default::default()
    };
    let queue_submit = need(f.queue_submit, "vkQueueSubmit")?;
    check(
        queue_submit(queue, 1, &submit, vk::Fence::from_raw(p.fence)),
        "vkQueueSubmit",
    )?;
    p.submitted = true;
    // the swapchain lists the fence so its destruction can wait on exactly this copy
    lock(&p.res.state).inflight_fences.push(p.fence);
    Ok(())
}

/// After the real present. Moves the capture to a readback thread; never blocks the present.
///
/// # Safety
/// `p` must come from [`begin`]; `present` is the result of the present it was rewritten into.
pub unsafe fn finish(mut p: Pending, present: vk::Result) {
    p.present_ok = matches!(present, vk::Result::SUCCESS | vk::Result::SUBOPTIMAL_KHR);
    let spawned = std::thread::Builder::new()
        .name("cs2mcp-readback".into())
        .spawn(move || {
            // a panic here must not leave the slot claimed: `p` is dropped during the unwind
            let _ = catch_unwind(AssertUnwindSafe(move || complete(p)));
        });
    match spawned {
        Ok(h) => {
            let mut jobs = lock(&JOBS);
            jobs.retain(|j| !j.is_finished());
            jobs.push(h);
        }
        // the closure (and the `Pending`) was dropped: its Drop deferred the GPU objects
        Err(e) => dlog!("cannot start readback thread: {e}"),
    }
}

fn complete(mut p: Pending) {
    // SAFETY: `p` owns the copy; `read_back` only waits and maps its own buffer.
    let outcome = unsafe { read_back(&mut p) };
    match outcome {
        Ok(bytes) => {
            let reusable = p.present_ok;
            // SAFETY: the copy finished (fence signalled), so the objects are idle.
            unsafe { p.release(true, reusable) };
            publish_pixels(&p, &bytes);
            p.done = true;
        }
        Err((e, gpu_done)) => {
            // SAFETY: as above; `gpu_done` decides between freeing and deferring.
            unsafe { p.release(gpu_done, false) };
            output::publish_error(&p.dir, &p.id, &e);
            p.done = true;
        }
    }
    // `p` drops here, releasing the IN_FLIGHT slot after the result is on disk
}

fn publish_pixels(p: &Pending, bytes: &[u8]) {
    let rgb = match convert::to_rgb(p.format, p.width, p.height, bytes) {
        Ok(rgb) => rgb,
        Err(e) => return output::publish_error(&p.dir, &p.id, &e),
    };
    let format = format!("{:?}", p.format);
    let meta = FrameMeta {
        id: &p.id,
        width: p.width,
        height: p.height,
        format: &format,
        present_index: p.present_index,
        ms: p.started_ms,
    };
    if let Err(e) = output::publish_frame(&p.dir, &meta, &rgb) {
        output::publish_error(&p.dir, &p.id, &e);
    }
}

/// Wait once for the copy, then map and copy the bytes out. The error says whether the GPU is
/// done with the objects.
unsafe fn read_back(p: &mut Pending) -> Result<Vec<u8>, (String, bool)> {
    let device = vk::Device::from_raw(p.dev.handle);
    let f = &p.dev.fns;
    let timeout = fence_timeout_ns();
    if timeout == 0 {
        // test knob: never wait, so every capture takes the deferred path
        return Err((
            "copy did not finish: waiting disabled by CS2MCP_CAPTURE_FENCE_TIMEOUT_MS=0".into(),
            false,
        ));
    }
    let wait = need(f.wait_for_fences, "vkWaitForFences").map_err(|e| (e, false))?;
    let fences = [vk::Fence::from_raw(p.fence)];
    let r = wait(device, 1, fences.as_ptr(), vk::TRUE, timeout);
    if r != vk::Result::SUCCESS {
        return Err((
            format!("copy did not finish: {r:?}"),
            r == vk::Result::ERROR_DEVICE_LOST,
        ));
    }
    let rb = p.readback.ok_or(("no readback buffer".to_string(), true))?;
    let memory = vk::DeviceMemory::from_raw(rb.memory);
    let map = need(f.map_memory, "vkMapMemory").map_err(|e| (e, true))?;
    let mut ptr: *mut c_void = null_mut();
    check(
        map(
            device,
            memory,
            0,
            rb.size,
            vk::MemoryMapFlags::empty(),
            &mut ptr,
        ),
        "vkMapMemory",
    )
    .map_err(|e| (e, true))?;
    if ptr.is_null() {
        return Err(("vkMapMemory returned NULL".into(), true));
    }
    if let Some(inval) = f.invalidate_mapped_memory_ranges {
        let range = vk::MappedMemoryRange {
            memory,
            offset: 0,
            size: vk::WHOLE_SIZE,
            ..Default::default()
        };
        let _ = inval(device, 1, &range);
    }
    let bytes = std::slice::from_raw_parts(ptr as *const u8, rb.size as usize).to_vec();
    if let Some(unmap) = f.unmap_memory {
        unmap(device, memory);
    }
    Ok(bytes)
}

/// Wait (bounded) for a set of fences to all signal. True if they did.
unsafe fn wait_fences(dev: &DeviceData, fences: &[u64], timeout_ns: u64) -> bool {
    let Some(wait) = dev.fns.wait_for_fences else {
        return false;
    };
    let handles: Vec<vk::Fence> = fences.iter().map(|&f| vk::Fence::from_raw(f)).collect();
    wait(
        vk::Device::from_raw(dev.handle),
        handles.len() as u32,
        handles.as_ptr(),
        vk::TRUE,
        timeout_ns,
    ) == vk::Result::SUCCESS
}

/// Move a swapchain's cached readbacks and parked semaphores to the device's lists (freed at
/// device destruction or by the next poll). Used when the swapchain cannot be waited on.
fn park_all(res: &SwapchainRes) {
    let (readbacks, sems) = {
        let mut st = lock(&res.state);
        st.alive = false;
        (
            std::mem::take(&mut st.readbacks),
            std::mem::take(&mut st.semaphores),
        )
    };
    lock(&res.dev.deferred).extend(readbacks.into_iter().map(|rb| Leftover {
        pool: 0,
        fence: 0,
        buffer: rb.buffer,
        memory: rb.memory,
        res: None,
    }));
    lock(&res.dev.retired).extend(sems.into_values());
}

/// Destroying a swapchain. If a copy from it is in flight, wait (bounded) for that copy's fence
/// only; never a device-wide idle, which needs external synchronisation of every queue. On timeout
/// the objects go to the deferred lists instead.
///
/// # Safety
/// Called from `vkDestroySwapchainKHR`, before the swapchain itself is destroyed.
pub unsafe fn release_swapchain(res: &SwapchainRes) {
    // Lock order: `res.state` may be held while taking the device's lists, never the reverse.
    // The fences stay listed while the wait runs, with `state` held: a readback thread (or a poll)
    // that frees one of them must first call `unlist_fence`, which blocks until this wait has
    // returned. So no fence is destroyed while it is being waited on.
    let (settled, readbacks, sems) = {
        let mut st = lock(&res.state);
        let settled = st.inflight_fences.is_empty()
            || wait_fences(&res.dev, &st.inflight_fences, SWAPCHAIN_WAIT_NS);
        // On timeout a queued copy may still run after the swapchain is gone: it reads the image
        // through its own fence, which is deferred until that fence signals.
        st.inflight_fences.clear();
        st.alive = false;
        (
            settled,
            std::mem::take(&mut st.readbacks),
            std::mem::take(&mut st.semaphores),
        )
    };
    // `state` is released before the device lists are taken
    if settled {
        for rb in readbacks {
            destroy_readback(&res.dev, rb);
        }
    } else {
        dlog!("swapchain destroyed with a copy still running; deferring its buffers");
        lock(&res.dev.deferred).extend(readbacks.into_iter().map(|rb| Leftover {
            pool: 0,
            fence: 0,
            buffer: rb.buffer,
            memory: rb.memory,
            res: None,
        }));
    }
    lock(&res.dev.retired).extend(sems.into_values());
}

/// Device teardown bookkeeping for a swapchain still registered when its device dies: its
/// objects are moved to the device lists, which `destroy_layer_objects` frees after a device idle.
pub fn park_swapchain_on_device_destroy(res: &SwapchainRes) {
    park_all(res);
}

impl DeviceData {
    /// Free deferred captures whose fence has signalled (called on every present).
    ///
    /// # Safety
    /// Only for the live device this data describes.
    pub unsafe fn poll_deferred(&self) {
        let mut list = lock(&self.deferred);
        if list.is_empty() {
            return;
        }
        let Some(status) = self.fns.get_fence_status else {
            return;
        };
        let device = vk::Device::from_raw(self.handle);
        let mut keep = Vec::new();
        for l in list.drain(..) {
            // no fence: nothing on the GPU refers to these objects any more
            let done = l.fence == 0 || {
                let r = status(device, vk::Fence::from_raw(l.fence));
                r == vk::Result::SUCCESS || r == vk::Result::ERROR_DEVICE_LOST
            };
            if done {
                free_leftover(self, l);
            } else {
                keep.push(l);
            }
        }
        *list = keep;
    }

    /// Device teardown: wait for all work, then free every deferred and parked object.
    ///
    /// # Safety
    /// Call before the device itself is destroyed.
    pub unsafe fn destroy_layer_objects(&self) {
        let device = vk::Device::from_raw(self.handle);
        if let Some(idle) = self.fns.device_wait_idle {
            let _ = idle(device);
        }
        let left: Vec<Leftover> = std::mem::take(&mut *lock(&self.deferred));
        for l in left {
            free_leftover(self, l);
        }
        let sems: Vec<u64> = std::mem::take(&mut *lock(&self.retired));
        for s in sems {
            if let Some(d) = self.fns.destroy_semaphore {
                d(device, vk::Semaphore::from_raw(s), null_mut());
            }
        }
    }
}

unsafe fn free_leftover(dev: &DeviceData, l: Leftover) {
    let device = vk::Device::from_raw(dev.handle);
    let f = &dev.fns;
    if let Some(res) = &l.res {
        unlist_fence(res, l.fence);
    }
    if l.buffer != 0 {
        if let Some(d) = f.destroy_buffer {
            d(device, vk::Buffer::from_raw(l.buffer), null_mut());
        }
    }
    if l.memory != 0 {
        if let Some(d) = f.free_memory {
            d(device, vk::DeviceMemory::from_raw(l.memory), null_mut());
        }
    }
    if l.fence != 0 {
        if let Some(d) = f.destroy_fence {
            d(device, vk::Fence::from_raw(l.fence), null_mut());
        }
    }
    if l.pool != 0 {
        if let Some(d) = f.destroy_command_pool {
            d(device, vk::CommandPool::from_raw(l.pool), null_mut());
        }
    }
}

/// Wait for queued readback threads (called before a device or instance is destroyed).
pub fn drain_jobs() {
    let handles: Vec<JoinHandle<()>> = std::mem::take(&mut *lock(&JOBS));
    for h in handles {
        let _ = h.join();
    }
}

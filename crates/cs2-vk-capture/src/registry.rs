//! Process-wide bookkeeping: which instance, physical device, device, queue and swapchain handle
//! belongs to what. Dispatchable parents are keyed by dispatch key, queues and swapchains by raw
//! handle. Locks are never held across calls into the driver except the non-blocking status poll.
use crate::dispatch::{dispatch_key, DeviceFns, Gdpa, Gipa};
use crate::layer::SetDeviceLoaderData;
use ash::vk::{self, Handle};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

pub struct InstanceData {
    pub handle: u64,
    pub gipa: Gipa,
    pub destroy: Option<vk::PFN_vkDestroyInstance>,
}

/// GPU objects of a capture that may still be in use. Freed once `fence` reports completion.
#[derive(Clone)]
pub struct Leftover {
    pub pool: u64,
    pub fence: u64,
    pub buffer: u64,
    pub memory: u64,
    /// The swapchain this copy read from, if any: freeing the fence also unlists it there.
    pub res: Option<Arc<SwapchainRes>>,
}

pub struct DeviceData {
    pub handle: u64,
    pub physical: u64,
    pub instance: u64,
    pub gipa: Gipa,
    pub gdpa: Gdpa,
    pub fns: DeviceFns,
    pub mem: vk::PhysicalDeviceMemoryProperties,
    /// Queue flags per family index, from the device's physical device.
    pub queue_flags: Vec<vk::QueueFlags>,
    /// The loader's data callback: objects this layer allocates are routed through it.
    pub set_loader: Option<SetDeviceLoaderData>,
    /// Captures whose GPU work did not finish in time. Checked on later presents.
    pub deferred: Mutex<Vec<Leftover>>,
    /// Layer semaphores parked until device destruction (a present may still wait on one).
    pub retired: Mutex<Vec<u64>>,
}

/// A host-visible readback buffer with its memory.
#[derive(Clone, Copy)]
pub struct Readback {
    pub buffer: u64,
    pub memory: u64,
    pub size: u64,
}

pub struct SwapState {
    /// False once the swapchain is destroyed: returned objects are then freed instead of cached.
    pub alive: bool,
    /// Fences of copies from this swapchain that may still be running (done, or deferred and not
    /// yet freed). Destruction waits on them, bounded.
    pub inflight_fences: Vec<u64>,
    pub readbacks: Vec<Readback>,
    /// One layer semaphore per image index, reused: re-acquiring an image implies the previous
    /// present finished waiting on it.
    pub semaphores: HashMap<u32, u64>,
}

/// Per-swapchain capture state, shared with the readback threads.
pub struct SwapchainRes {
    pub dev: Arc<DeviceData>,
    pub state: Mutex<SwapState>,
}

impl SwapchainRes {
    pub fn new(dev: Arc<DeviceData>) -> Self {
        SwapchainRes {
            dev,
            state: Mutex::new(SwapState {
                alive: true,
                inflight_fences: Vec::new(),
                readbacks: Vec::new(),
                semaphores: HashMap::new(),
            }),
        }
    }
}

#[derive(Clone)]
pub struct SwapchainInfo {
    pub device: usize,
    pub format: vk::Format,
    pub color_space: vk::ColorSpaceKHR,
    pub extent: (u32, u32),
    pub capturable: bool,
    /// Raw `VkImage` handles from `vkGetSwapchainImagesKHR`.
    pub images: Vec<u64>,
    pub res: Arc<SwapchainRes>,
}

/// The swapchain image a present will show, and what the capture needs to know about it.
pub struct Target {
    pub res: Arc<SwapchainRes>,
    pub image: u64,
    pub index: u32,
    pub format: vk::Format,
    pub color_space: vk::ColorSpaceKHR,
    pub extent: (u32, u32),
}

#[derive(Clone, Copy)]
struct QueueInfo {
    device: usize,
    family: u32,
}

#[derive(Default)]
struct Registry {
    instances: HashMap<usize, Arc<InstanceData>>,
    /// physical device dispatch key -> instance key
    physical: HashMap<usize, usize>,
    devices: HashMap<usize, Arc<DeviceData>>,
    queues: HashMap<u64, QueueInfo>,
    swapchains: HashMap<u64, SwapchainInfo>,
}

static REG: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

fn lock() -> MutexGuard<'static, Registry> {
    REG.lock().unwrap_or_else(|e| e.into_inner())
}

impl DeviceData {
    pub fn new(
        (handle, physical, instance): (u64, u64, u64),
        gipa: Gipa,
        gdpa: Gdpa,
        mem: vk::PhysicalDeviceMemoryProperties,
        queue_flags: Vec<vk::QueueFlags>,
        set_loader: Option<SetDeviceLoaderData>,
    ) -> Self {
        // SAFETY: `handle` is the device the driver just returned to us.
        let fns = unsafe { DeviceFns::load(gdpa, vk::Device::from_raw(handle)) };
        DeviceData {
            handle,
            physical,
            instance,
            gipa,
            gdpa,
            fns,
            mem,
            queue_flags,
            set_loader,
            deferred: Mutex::new(Vec::new()),
            retired: Mutex::new(Vec::new()),
        }
    }

    /// Graphics, compute or transfer: the families a copy can run on.
    pub fn queue_capturable(&self, family: u32) -> bool {
        self.queue_flags.get(family as usize).is_some_and(|f| {
            f.intersects(
                vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER,
            )
        })
    }
}

pub fn add_instance(handle: u64, gipa: Gipa, destroy: Option<vk::PFN_vkDestroyInstance>) {
    let data = Arc::new(InstanceData {
        handle,
        gipa,
        destroy,
    });
    lock().instances.insert(dispatch_key(handle), data);
}

pub fn instance(handle: u64) -> Option<Arc<InstanceData>> {
    lock().instances.get(&dispatch_key(handle)).cloned()
}

pub fn remove_instance(handle: u64) -> Option<Arc<InstanceData>> {
    let key = dispatch_key(handle);
    let mut reg = lock();
    reg.physical.retain(|_, inst| *inst != key);
    reg.instances.remove(&key)
}

/// Record the physical devices an instance enumerated (keyed by their dispatch key).
pub fn note_physical(devices: &[u64], instance: u64) {
    let key = dispatch_key(instance);
    let mut reg = lock();
    for &pd in devices {
        reg.physical.insert(dispatch_key(pd), key);
    }
}

/// The instance a physical device was enumerated from. `None` if it was never enumerated through
/// the layer (then the layer cannot resolve the next `vkCreateDevice` and reports an error).
pub fn instance_for_physical(physical: u64) -> Option<Arc<InstanceData>> {
    let reg = lock();
    let key = *reg.physical.get(&dispatch_key(physical))?;
    reg.instances.get(&key).cloned()
}

pub fn add_device(dev: Arc<DeviceData>) {
    lock().devices.insert(dispatch_key(dev.handle), dev);
}

pub fn device(handle: u64) -> Option<Arc<DeviceData>> {
    lock().devices.get(&dispatch_key(handle)).cloned()
}

/// Forget a device and the queues and swapchains registered on it.
pub fn remove_device(handle: u64) -> Option<Arc<DeviceData>> {
    let key = dispatch_key(handle);
    let mut reg = lock();
    reg.queues.retain(|_, q| q.device != key);
    // swapchains still registered on this device: their objects go to the device's lists, which
    // `destroy_layer_objects` frees after a device idle
    reg.swapchains.retain(|_, s| {
        if s.device == key {
            crate::capture::park_swapchain_on_device_destroy(&s.res);
            false
        } else {
            true
        }
    });
    reg.devices.remove(&key)
}

pub fn add_queue(queue: u64, device: u64, family: u32) {
    let info = QueueInfo {
        device: dispatch_key(device),
        family,
    };
    lock().queues.insert(queue, info);
}

/// The device of a queue and, when the layer saw the queue come out of `vkGetDeviceQueue(2)`, its
/// family. An untracked queue still maps to its device through the dispatch key, so the present
/// can be forwarded (it just is not captured).
pub fn device_for_queue(queue: u64) -> Option<(Arc<DeviceData>, Option<u32>)> {
    let reg = lock();
    if let Some(q) = reg.queues.get(&queue) {
        let dev = reg.devices.get(&q.device)?.clone();
        return Some((dev, Some(q.family)));
    }
    let dev = reg.devices.get(&dispatch_key(queue))?.clone();
    Some((dev, None))
}

pub fn add_swapchain(handle: u64, info: SwapchainInfo) {
    lock().swapchains.insert(handle, info);
}

pub fn remove_swapchain(handle: u64) -> Option<SwapchainInfo> {
    lock().swapchains.remove(&handle)
}

pub fn set_swapchain_images(handle: u64, images: Vec<u64>) {
    if let Some(s) = lock().swapchains.get_mut(&handle) {
        s.images = images;
    }
}

/// The first capturable swapchain of a present, and the image it shows. Only the handles and a
/// clone of one `Arc` are copied out of the lock.
pub fn find_target(swapchains: &[u64], indices: &[u32]) -> Option<Target> {
    let reg = lock();
    swapchains.iter().zip(indices).find_map(|(&sc, &index)| {
        let s = reg.swapchains.get(&sc)?;
        if !s.capturable {
            return None;
        }
        let image = *s.images.get(index as usize)?;
        Some(Target {
            res: s.res.clone(),
            image,
            index,
            format: s.format,
            color_space: s.color_space,
            extent: s.extent,
        })
    })
}

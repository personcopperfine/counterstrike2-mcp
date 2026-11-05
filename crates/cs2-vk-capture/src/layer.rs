//! The layer entry points. Only `vkNegotiateLoaderLayerInterfaceVersion` is exported; everything
//! else is handed out through the GIPA/GDPA it returns, so the loader keeps the chain intact.
//!
//! Rules for every entry point: a panic is caught and never crosses FFI; a handle the layer does
//! not know is passed through (or, when there is no next function to call, the call returns the
//! Vulkan error the loader would give); and the driver's result is returned even if the layer's
//! own bookkeeping after the call fails.
#![allow(non_snake_case)]

use crate::capture;
use crate::dispatch::{self, Gdpa, Gipa};
use crate::registry::{self, DeviceData, SwapchainInfo, SwapchainRes};
use ash::vk::{self, Handle};
use std::ffi::{c_char, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::null_mut;
use std::sync::Arc;

const LAYER_NEGOTIATE_INTERFACE_STRUCT: u32 = 1;
const LOADER_INSTANCE_CREATE_INFO: i32 = 47;
const LOADER_DEVICE_CREATE_INFO: i32 = 48;
const VK_LAYER_LINK_INFO: u32 = 0;
const LOADER_DATA_CALLBACK: u32 = 1;
const IMAGE_FORMAT_LIST_CREATE_INFO: i32 = 1_000_147_000;
/// Highest loader-layer interface version this layer implements.
const MAX_INTERFACE_VERSION: u32 = 2;

/// Instance-level entry points the layer intercepts.
const INSTANCE_LEVEL: &[&str] = &[
    "vkDestroyInstance",
    "vkEnumeratePhysicalDevices",
    "vkEnumeratePhysicalDeviceGroups",
    "vkCreateDevice",
];
/// Device-level entry points the layer intercepts.
const DEVICE_LEVEL: &[&str] = &[
    "vkDestroyDevice",
    "vkGetDeviceQueue",
    "vkGetDeviceQueue2",
    "vkCreateSwapchainKHR",
    "vkGetSwapchainImagesKHR",
    "vkDestroySwapchainKHR",
    "vkQueuePresentKHR",
];

/// `VkNegotiateLayerInterface`.
#[repr(C)]
pub struct NegotiateLayerInterface {
    s_type: u32,
    p_next: *const c_void,
    loader_layer_interface_version: u32,
    get_instance_proc_addr: Option<Gipa>,
    get_device_proc_addr: Option<Gdpa>,
    get_physical_device_proc_addr: Option<Gipa>,
}

/// Header of the loader's `VkLayer{Instance,Device}CreateInfo`.
#[repr(C)]
struct LoaderCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    function: u32,
    link: *mut c_void,
}

/// `VkLayerInstanceLink`.
#[repr(C)]
struct InstanceLink {
    next: *mut InstanceLink,
    next_gipa: Gipa,
    next_get_physical_device_proc_addr: *const c_void,
}

/// `VkLayerDeviceLink`.
#[repr(C)]
struct DeviceLink {
    next: *mut DeviceLink,
    next_gipa: Gipa,
    next_gdpa: Gdpa,
}

/// `VK_LOADER_DATA_CALLBACK`: the loader's `vkSetDeviceLoaderData`, which a layer must call on
/// every dispatchable object it creates itself (here: command buffers) so the loader can route it.
pub type SetDeviceLoaderData = unsafe extern "system" fn(vk::Device, *mut c_void) -> vk::Result;

/// Generic structure header (every Vulkan struct starts with `sType` and `pNext`).
#[repr(C)]
struct Header {
    s_type: i32,
    p_next: *const c_void,
}

/// The loader's data callback node in a device create chain (`function == LOADER_DATA_CALLBACK`).
unsafe fn find_loader_callback(mut p: *const c_void) -> Option<SetDeviceLoaderData> {
    while !p.is_null() {
        let c = p as *mut LoaderCreateInfo;
        if (*c).s_type == LOADER_DEVICE_CREATE_INFO && (*c).function == LOADER_DATA_CALLBACK {
            let f = (*c).link;
            return if f.is_null() {
                None
            } else {
                Some(std::mem::transmute::<*mut c_void, SetDeviceLoaderData>(f))
            };
        }
        p = (*c).p_next;
    }
    None
}

/// Run `f`; on panic return `fallback`.
fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            dlog!("panic caught in a layer entry point");
            fallback
        }
    }
}

/// The loader's link-info node in a pNext chain.
unsafe fn find_link(mut p: *const c_void, s_type: i32) -> *mut LoaderCreateInfo {
    while !p.is_null() {
        let c = p as *mut LoaderCreateInfo;
        if (*c).s_type == s_type && (*c).function == VK_LAYER_LINK_INFO {
            return c;
        }
        p = (*c).p_next;
    }
    null_mut()
}

macro_rules! intercepts {
    ($($name:literal => $f:ident),* $(,)?) => {
        /// The entry point this layer replaces for `name`, if any.
        fn intercept(name: &str) -> vk::PFN_vkVoidFunction {
            let p: *const () = match name {
                $($name => $f as *const (),)*
                _ => return None,
            };
            // SAFETY: a non-null function pointer has the size of `Option<fn>`.
            unsafe { std::mem::transmute::<*const (), vk::PFN_vkVoidFunction>(p) }
        }
    };
}

intercepts! {
    "vkGetInstanceProcAddr" => vkGetInstanceProcAddr,
    "vkGetDeviceProcAddr" => vkGetDeviceProcAddr,
    "vkCreateInstance" => vkCreateInstance,
    "vkDestroyInstance" => vkDestroyInstance,
    "vkEnumeratePhysicalDevices" => vkEnumeratePhysicalDevices,
    "vkEnumeratePhysicalDeviceGroups" => vkEnumeratePhysicalDeviceGroups,
    "vkCreateDevice" => vkCreateDevice,
    "vkDestroyDevice" => vkDestroyDevice,
    "vkGetDeviceQueue" => vkGetDeviceQueue,
    "vkGetDeviceQueue2" => vkGetDeviceQueue2,
    "vkCreateSwapchainKHR" => vkCreateSwapchainKHR,
    "vkGetSwapchainImagesKHR" => vkGetSwapchainImagesKHR,
    "vkDestroySwapchainKHR" => vkDestroySwapchainKHR,
    "vkQueuePresentKHR" => vkQueuePresentKHR,
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn vkNegotiateLoaderLayerInterfaceVersion(
    p_version: *mut NegotiateLayerInterface,
) -> vk::Result {
    guard(vk::Result::ERROR_INITIALIZATION_FAILED, || {
        if p_version.is_null() {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        let v = &mut *p_version;
        if v.s_type == LAYER_NEGOTIATE_INTERFACE_STRUCT || v.s_type == 0 {
            if v.loader_layer_interface_version > MAX_INTERFACE_VERSION {
                v.loader_layer_interface_version = MAX_INTERFACE_VERSION;
            }
            v.get_instance_proc_addr = Some(vkGetInstanceProcAddr);
            v.get_device_proc_addr = Some(vkGetDeviceProcAddr);
            v.get_physical_device_proc_addr = None;
        }
        vk::Result::SUCCESS
    })
}

unsafe extern "system" fn vkGetInstanceProcAddr(
    instance: vk::Instance,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    guard(None, || {
        if p_name.is_null() {
            return None;
        }
        let name = CStr::from_ptr(p_name).to_str().ok()?;
        if name == "vkCreateInstance"
            || name == "vkGetInstanceProcAddr"
            || name == "vkGetDeviceProcAddr"
        {
            return intercept(name);
        }
        let inst = registry::instance(instance.as_raw())?;
        // the next layer must know the name too, or the layer would advertise a dead entry point
        let next = (inst.gipa)(instance, p_name)?;
        if INSTANCE_LEVEL.contains(&name) || DEVICE_LEVEL.contains(&name) {
            return intercept(name);
        }
        Some(next)
    })
}

unsafe extern "system" fn vkGetDeviceProcAddr(
    device: vk::Device,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    guard(None, || {
        if p_name.is_null() {
            return None;
        }
        let name = CStr::from_ptr(p_name).to_str().ok()?;
        if name == "vkGetDeviceProcAddr" {
            return intercept(name);
        }
        // instance-level names are not device-level: NULL
        if INSTANCE_LEVEL.contains(&name)
            || name == "vkCreateInstance"
            || name == "vkGetInstanceProcAddr"
        {
            return None;
        }
        let dev = registry::device(device.as_raw())?;
        let next = (dev.gdpa)(device, p_name)?;
        if DEVICE_LEVEL.contains(&name) {
            return intercept(name);
        }
        Some(next)
    })
}

unsafe extern "system" fn vkCreateInstance(
    p_create_info: *const vk::InstanceCreateInfo,
    p_allocator: *const vk::AllocationCallbacks,
    p_instance: *mut vk::Instance,
) -> vk::Result {
    // pre-call: find our link and advance the chain
    let pre = guard(None, || {
        if p_create_info.is_null() || p_instance.is_null() {
            return None;
        }
        let info = find_link((*p_create_info).p_next, LOADER_INSTANCE_CREATE_INFO);
        if info.is_null() {
            return None;
        }
        let link = (*info).link as *mut InstanceLink;
        let next_gipa = (*link).next_gipa;
        (*info).link = (*link).next as *mut c_void;
        let create = dispatch::inst_fn::<vk::PFN_vkCreateInstance>(
            next_gipa,
            vk::Instance::null(),
            "vkCreateInstance",
        )?;
        Some((create, next_gipa))
    });
    let Some((create, next_gipa)) = pre else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let r = create(p_create_info, p_allocator, p_instance);
    if r == vk::Result::SUCCESS {
        // post-call: must never change the result of a created instance
        guard((), || {
            let instance = *p_instance;
            let destroy = dispatch::inst_fn::<vk::PFN_vkDestroyInstance>(
                next_gipa,
                instance,
                "vkDestroyInstance",
            );
            registry::add_instance(instance.as_raw(), next_gipa, destroy);
            dlog!("instance {:#x} created", instance.as_raw());
        });
    }
    r
}

unsafe extern "system" fn vkDestroyInstance(
    instance: vk::Instance,
    p_allocator: *const vk::AllocationCallbacks,
) {
    let data = guard(None, || registry::remove_instance(instance.as_raw()));
    let Some(inst) = data else { return };
    guard((), capture::drain_jobs);
    if let Some(destroy) = inst.destroy {
        destroy(instance, p_allocator);
    }
}

unsafe extern "system" fn vkEnumeratePhysicalDevices(
    instance: vk::Instance,
    p_count: *mut u32,
    p_devices: *mut vk::PhysicalDevice,
) -> vk::Result {
    let next = guard(None, || {
        let inst = registry::instance(instance.as_raw())?;
        dispatch::inst_fn::<vk::PFN_vkEnumeratePhysicalDevices>(
            inst.gipa,
            instance,
            "vkEnumeratePhysicalDevices",
        )
    });
    let Some(f) = next else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let r = f(instance, p_count, p_devices);
    if (r == vk::Result::SUCCESS || r == vk::Result::INCOMPLETE)
        && !p_count.is_null()
        && !p_devices.is_null()
    {
        guard((), || {
            let pds: Vec<u64> = std::slice::from_raw_parts(p_devices, *p_count as usize)
                .iter()
                .map(|pd| pd.as_raw())
                .collect();
            registry::note_physical(&pds, instance.as_raw());
        });
    }
    r
}

unsafe extern "system" fn vkEnumeratePhysicalDeviceGroups(
    instance: vk::Instance,
    p_count: *mut u32,
    p_props: *mut vk::PhysicalDeviceGroupProperties,
) -> vk::Result {
    let next = guard(None, || {
        let inst = registry::instance(instance.as_raw())?;
        dispatch::inst_fn::<vk::PFN_vkEnumeratePhysicalDeviceGroups>(
            inst.gipa,
            instance,
            "vkEnumeratePhysicalDeviceGroups",
        )
    });
    let Some(f) = next else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let r = f(instance, p_count, p_props);
    if (r == vk::Result::SUCCESS || r == vk::Result::INCOMPLETE)
        && !p_count.is_null()
        && !p_props.is_null()
    {
        guard((), || {
            for g in std::slice::from_raw_parts(p_props, *p_count as usize) {
                let pds: Vec<u64> = g.physical_devices[..g.physical_device_count as usize]
                    .iter()
                    .map(|pd| pd.as_raw())
                    .collect();
                registry::note_physical(&pds, instance.as_raw());
            }
        });
    }
    r
}

unsafe extern "system" fn vkCreateDevice(
    physical_device: vk::PhysicalDevice,
    p_create_info: *const vk::DeviceCreateInfo,
    p_allocator: *const vk::AllocationCallbacks,
    p_device: *mut vk::Device,
) -> vk::Result {
    let pre = guard(None, || {
        if p_create_info.is_null() || p_device.is_null() {
            return None;
        }
        let info = find_link((*p_create_info).p_next, LOADER_DEVICE_CREATE_INFO);
        if info.is_null() {
            return None;
        }
        let link = (*info).link as *mut DeviceLink;
        let next_gipa = (*link).next_gipa;
        let next_gdpa = (*link).next_gdpa;
        (*info).link = (*link).next as *mut c_void;
        let set_loader = find_loader_callback((*p_create_info).p_next);
        // no fallback instance: the physical device must come from our own enumeration
        let inst = registry::instance_for_physical(physical_device.as_raw())?;
        let create = dispatch::inst_fn::<vk::PFN_vkCreateDevice>(
            next_gipa,
            vk::Instance::from_raw(inst.handle),
            "vkCreateDevice",
        )?;
        Some((create, inst, next_gipa, next_gdpa, set_loader))
    });
    let Some((create, inst, next_gipa, next_gdpa, set_loader)) = pre else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let r = create(physical_device, p_create_info, p_allocator, p_device);
    if r == vk::Result::SUCCESS {
        guard((), || {
            let device = *p_device;
            let instance = vk::Instance::from_raw(inst.handle);
            let pd = physical_device;
            let mut mem = vk::PhysicalDeviceMemoryProperties::default();
            if let Some(f) = dispatch::inst_fn::<vk::PFN_vkGetPhysicalDeviceMemoryProperties>(
                next_gipa,
                instance,
                "vkGetPhysicalDeviceMemoryProperties",
            ) {
                f(pd, &mut mem);
            }
            let mut flags = Vec::new();
            if let Some(f) = dispatch::inst_fn::<vk::PFN_vkGetPhysicalDeviceQueueFamilyProperties>(
                next_gipa,
                instance,
                "vkGetPhysicalDeviceQueueFamilyProperties",
            ) {
                let mut n = 0u32;
                f(pd, &mut n, null_mut());
                let mut props = vec![vk::QueueFamilyProperties::default(); n as usize];
                f(pd, &mut n, props.as_mut_ptr());
                flags = props.iter().map(|p| p.queue_flags).collect();
            }
            let data = DeviceData::new(
                (device.as_raw(), pd.as_raw(), inst.handle),
                next_gipa,
                next_gdpa,
                mem,
                flags,
                set_loader,
            );
            registry::add_device(Arc::new(data));
        });
    }
    r
}

unsafe extern "system" fn vkDestroyDevice(
    device: vk::Device,
    p_allocator: *const vk::AllocationCallbacks,
) {
    let data: Option<Arc<DeviceData>> = guard(None, || registry::remove_device(device.as_raw()));
    let Some(dev) = data else { return };
    guard((), || {
        capture::drain_jobs();
        dev.destroy_layer_objects();
    });
    if let Some(destroy) = dev.fns.destroy_device {
        destroy(device, p_allocator);
    }
}

unsafe extern "system" fn vkGetDeviceQueue(
    device: vk::Device,
    queue_family_index: u32,
    queue_index: u32,
    p_queue: *mut vk::Queue,
) {
    let Some(next) = guard(None, || {
        registry::device(device.as_raw()).and_then(|d| d.fns.get_device_queue)
    }) else {
        return;
    };
    next(device, queue_family_index, queue_index, p_queue);
    guard((), || {
        if !p_queue.is_null() && !(*p_queue).is_null() {
            registry::add_queue((*p_queue).as_raw(), device.as_raw(), queue_family_index);
        }
    });
}

unsafe extern "system" fn vkGetDeviceQueue2(
    device: vk::Device,
    p_queue_info: *const vk::DeviceQueueInfo2,
    p_queue: *mut vk::Queue,
) {
    let Some(next) = guard(None, || {
        registry::device(device.as_raw()).and_then(|d| d.fns.get_device_queue2)
    }) else {
        return;
    };
    next(device, p_queue_info, p_queue);
    guard((), || {
        if !p_queue.is_null() && !(*p_queue).is_null() && !p_queue_info.is_null() {
            let family = (*p_queue_info).queue_family_index;
            registry::add_queue((*p_queue).as_raw(), device.as_raw(), family);
        }
    });
}

unsafe extern "system" fn vkCreateSwapchainKHR(
    device: vk::Device,
    p_create_info: *const vk::SwapchainCreateInfoKHR,
    p_allocator: *const vk::AllocationCallbacks,
    p_swapchain: *mut vk::SwapchainKHR,
) -> vk::Result {
    let pre = guard(None, || {
        if p_create_info.is_null() {
            return None;
        }
        let dev = registry::device(device.as_raw())?;
        let create = dev.fns.create_swapchain?;
        let mut info = *p_create_info;
        // Adds TRANSFER_SRC only when every check passes; otherwise the swapchain is passed through
        // unchanged and is never captured.
        let capturable = guard(false, || decide_capturable(&dev, &mut info));
        Some((dev, create, info, capturable))
    });
    let Some((dev, create, info, capturable)) = pre else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let r = create(device, &info, p_allocator, p_swapchain);
    if r == vk::Result::SUCCESS {
        guard((), || {
            let entry = SwapchainInfo {
                device: dispatch::dispatch_key(device.as_raw()),
                format: info.image_format,
                color_space: info.image_color_space,
                extent: (info.image_extent.width, info.image_extent.height),
                capturable,
                images: Vec::new(),
                res: Arc::new(SwapchainRes::new(dev.clone())),
            };
            registry::add_swapchain((*p_swapchain).as_raw(), entry);
            dlog!(
                "swapchain {:#x} {:?} {}x{} capturable={capturable}",
                (*p_swapchain).as_raw(),
                info.image_format,
                info.image_extent.width,
                info.image_extent.height
            );
        });
    }
    r
}

/// Whether this swapchain can be captured. On success `info.image_usage` gains TRANSFER_SRC.
///
/// Refused: protected swapchains, shared present modes, and any combination the driver cannot
/// create with TRANSFER_SRC (checked with `vkGetPhysicalDeviceImageFormatProperties2`, honouring
/// a `VkImageFormatListCreateInfo` on a mutable-format swapchain).
unsafe fn decide_capturable(dev: &DeviceData, info: &mut vk::SwapchainCreateInfoKHR) -> bool {
    if info.flags.contains(vk::SwapchainCreateFlagsKHR::PROTECTED) {
        return false;
    }
    if matches!(
        info.present_mode,
        vk::PresentModeKHR::SHARED_DEMAND_REFRESH | vk::PresentModeKHR::SHARED_CONTINUOUS_REFRESH
    ) {
        return false;
    }
    let instance = vk::Instance::from_raw(dev.instance);
    let pd = vk::PhysicalDevice::from_raw(dev.physical);
    let Some(caps_fn) = dispatch::inst_fn::<vk::PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR>(
        dev.gipa,
        instance,
        "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
    ) else {
        return false;
    };
    let mut caps = vk::SurfaceCapabilitiesKHR::default();
    if caps_fn(pd, info.surface, &mut caps) != vk::Result::SUCCESS
        || !caps
            .supported_usage_flags
            .contains(vk::ImageUsageFlags::TRANSFER_SRC)
    {
        return false;
    }
    let usage = info.image_usage | vk::ImageUsageFlags::TRANSFER_SRC;
    let Some(format_fn) = dispatch::inst_fn::<vk::PFN_vkGetPhysicalDeviceImageFormatProperties2>(
        dev.gipa,
        instance,
        "vkGetPhysicalDeviceImageFormatProperties2",
    ) else {
        return false;
    };
    let mutable = info
        .flags
        .contains(vk::SwapchainCreateFlagsKHR::MUTABLE_FORMAT);
    // A private copy of the list with its own pNext cleared: the swapchain's chain must not be
    // forwarded into the query, and the copy lives until the query returns.
    let list_copy = find_format_list(info.p_next).map(|p| {
        let mut list = *(p as *const vk::ImageFormatListCreateInfo);
        list.p_next = std::ptr::null();
        list
    });
    let fi = vk::PhysicalDeviceImageFormatInfo2 {
        s_type: vk::StructureType::PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
        p_next: list_copy
            .as_ref()
            .map_or(std::ptr::null(), |l| l as *const _ as *const c_void),
        format: info.image_format,
        ty: vk::ImageType::TYPE_2D,
        tiling: vk::ImageTiling::OPTIMAL,
        usage,
        flags: if mutable {
            vk::ImageCreateFlags::MUTABLE_FORMAT
        } else {
            vk::ImageCreateFlags::empty()
        },
        ..Default::default()
    };
    let mut props = vk::ImageFormatProperties2::default();
    if format_fn(pd, &fi, &mut props) != vk::Result::SUCCESS {
        return false;
    }
    info.image_usage = usage;
    true
}

/// The `VkImageFormatListCreateInfo` in a swapchain's pNext chain, if any.
unsafe fn find_format_list(mut p: *const c_void) -> Option<*const c_void> {
    while !p.is_null() {
        let h = p as *const Header;
        if (*h).s_type == IMAGE_FORMAT_LIST_CREATE_INFO {
            return Some(p);
        }
        p = (*h).p_next;
    }
    None
}

unsafe extern "system" fn vkGetSwapchainImagesKHR(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    p_count: *mut u32,
    p_images: *mut vk::Image,
) -> vk::Result {
    let next = guard(None, || {
        registry::device(device.as_raw()).and_then(|d| d.fns.get_swapchain_images)
    });
    let Some(f) = next else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let r = f(device, swapchain, p_count, p_images);
    if (r == vk::Result::SUCCESS || r == vk::Result::INCOMPLETE)
        && !p_count.is_null()
        && !p_images.is_null()
    {
        guard((), || {
            let images: Vec<u64> = std::slice::from_raw_parts(p_images, *p_count as usize)
                .iter()
                .map(|i| i.as_raw())
                .collect();
            registry::set_swapchain_images(swapchain.as_raw(), images);
        });
    }
    r
}

unsafe extern "system" fn vkDestroySwapchainKHR(
    device: vk::Device,
    swapchain: vk::SwapchainKHR,
    p_allocator: *const vk::AllocationCallbacks,
) {
    let next = guard(None, || {
        let info = registry::remove_swapchain(swapchain.as_raw());
        if let Some(info) = &info {
            capture::release_swapchain(&info.res);
        }
        registry::device(device.as_raw())?.fns.destroy_swapchain
    });
    if let Some(f) = next {
        f(device, swapchain, p_allocator);
    }
}

unsafe extern "system" fn vkQueuePresentKHR(
    queue: vk::Queue,
    p_present_info: *const vk::PresentInfoKHR,
) -> vk::Result {
    // Without a device there is no next function to call: the only honest answer is an error.
    let Some((dev, family)) = guard(None, || registry::device_for_queue(queue.as_raw())) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let Some(next) = dev.fns.queue_present else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    if p_present_info.is_null() {
        return next(queue, p_present_info);
    }
    guard((), || dev.poll_deferred());
    let pending = capture::begin(queue, &dev, family, &*p_present_info);
    let result = match &pending {
        Some(p) => {
            let rewritten = p.rewrite(&*p_present_info);
            next(queue, &rewritten)
        }
        None => next(queue, p_present_info),
    };
    if let Some(p) = pending {
        guard((), || capture::finish(p, result));
    }
    result
}

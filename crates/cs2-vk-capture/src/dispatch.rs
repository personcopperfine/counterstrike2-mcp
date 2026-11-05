//! Loading next-in-chain Vulkan functions, and the table of device functions the layer calls.
use ash::vk;
use std::ffi::CString;

pub type Gipa = vk::PFN_vkGetInstanceProcAddr;
pub type Gdpa = vk::PFN_vkGetDeviceProcAddr;

/// Key of a dispatchable object: the loader writes a dispatch-table pointer into its first word.
pub fn dispatch_key(handle: u64) -> usize {
    if handle == 0 {
        return 0;
    }
    // SAFETY: the caller passes a live dispatchable handle, i.e. a pointer to a table pointer.
    unsafe { *(handle as usize as *const usize) }
}

unsafe fn cast<T: Copy>(f: vk::PFN_vkVoidFunction) -> Option<T> {
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<usize>());
    f.map(|f| std::mem::transmute_copy::<unsafe extern "system" fn(), T>(&f))
}

/// Resolve an instance-level function through `gipa`. `T` is a `vk::PFN_*` type.
pub unsafe fn inst_fn<T: Copy>(gipa: Gipa, instance: vk::Instance, name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    cast(gipa(instance, c.as_ptr()))
}

/// Resolve a device-level function through `gdpa`.
pub unsafe fn dev_fn<T: Copy>(gdpa: Gdpa, device: vk::Device, name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    cast(gdpa(device, c.as_ptr()))
}

/// Device functions the layer calls on the next layer/driver. Each is optional: a missing entry
/// makes the capture fail with `{id, error}`; it never changes the game's own calls.
macro_rules! device_fns {
    ($($field:ident: $pfn:ty = $sym:literal),* $(,)?) => {
        #[derive(Clone, Copy)]
        pub struct DeviceFns {
            $(pub $field: Option<$pfn>,)*
        }

        impl DeviceFns {
            pub unsafe fn load(gdpa: Gdpa, device: vk::Device) -> Self {
                Self { $($field: dev_fn::<$pfn>(gdpa, device, $sym),)* }
            }
        }
    };
}

device_fns! {
    destroy_device: vk::PFN_vkDestroyDevice = "vkDestroyDevice",
    device_wait_idle: vk::PFN_vkDeviceWaitIdle = "vkDeviceWaitIdle",
    get_device_queue: vk::PFN_vkGetDeviceQueue = "vkGetDeviceQueue",
    get_device_queue2: vk::PFN_vkGetDeviceQueue2 = "vkGetDeviceQueue2",
    create_swapchain: vk::PFN_vkCreateSwapchainKHR = "vkCreateSwapchainKHR",
    destroy_swapchain: vk::PFN_vkDestroySwapchainKHR = "vkDestroySwapchainKHR",
    get_swapchain_images: vk::PFN_vkGetSwapchainImagesKHR = "vkGetSwapchainImagesKHR",
    queue_present: vk::PFN_vkQueuePresentKHR = "vkQueuePresentKHR",
    create_command_pool: vk::PFN_vkCreateCommandPool = "vkCreateCommandPool",
    destroy_command_pool: vk::PFN_vkDestroyCommandPool = "vkDestroyCommandPool",
    allocate_command_buffers: vk::PFN_vkAllocateCommandBuffers = "vkAllocateCommandBuffers",
    begin_command_buffer: vk::PFN_vkBeginCommandBuffer = "vkBeginCommandBuffer",
    end_command_buffer: vk::PFN_vkEndCommandBuffer = "vkEndCommandBuffer",
    cmd_pipeline_barrier: vk::PFN_vkCmdPipelineBarrier = "vkCmdPipelineBarrier",
    cmd_copy_image_to_buffer: vk::PFN_vkCmdCopyImageToBuffer = "vkCmdCopyImageToBuffer",
    queue_submit: vk::PFN_vkQueueSubmit = "vkQueueSubmit",
    create_buffer: vk::PFN_vkCreateBuffer = "vkCreateBuffer",
    destroy_buffer: vk::PFN_vkDestroyBuffer = "vkDestroyBuffer",
    get_buffer_memory_requirements: vk::PFN_vkGetBufferMemoryRequirements = "vkGetBufferMemoryRequirements",
    bind_buffer_memory: vk::PFN_vkBindBufferMemory = "vkBindBufferMemory",
    allocate_memory: vk::PFN_vkAllocateMemory = "vkAllocateMemory",
    free_memory: vk::PFN_vkFreeMemory = "vkFreeMemory",
    map_memory: vk::PFN_vkMapMemory = "vkMapMemory",
    unmap_memory: vk::PFN_vkUnmapMemory = "vkUnmapMemory",
    invalidate_mapped_memory_ranges: vk::PFN_vkInvalidateMappedMemoryRanges = "vkInvalidateMappedMemoryRanges",
    create_fence: vk::PFN_vkCreateFence = "vkCreateFence",
    destroy_fence: vk::PFN_vkDestroyFence = "vkDestroyFence",
    get_fence_status: vk::PFN_vkGetFenceStatus = "vkGetFenceStatus",
    wait_for_fences: vk::PFN_vkWaitForFences = "vkWaitForFences",
    create_semaphore: vk::PFN_vkCreateSemaphore = "vkCreateSemaphore",
    destroy_semaphore: vk::PFN_vkDestroySemaphore = "vkDestroySemaphore",
}

/// `Option` to error, naming the missing entry point.
pub fn need<T>(f: Option<T>, name: &str) -> Result<T, String> {
    f.ok_or_else(|| format!("{name} is not available"))
}

/// Map a `VkResult` to `Result`, naming the failed step.
pub fn check(r: vk::Result, what: &str) -> Result<(), String> {
    if r == vk::Result::SUCCESS {
        Ok(())
    } else {
        Err(format!("{what} failed: {r:?}"))
    }
}

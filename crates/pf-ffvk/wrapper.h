/* The one header ffmpeg-sys-next's bindgen list omits: FFmpeg's Vulkan hwcontext.
 * Pulls <vulkan/vulkan.h> (the vulkan-headers package) transitively. */
#include <libavutil/hwcontext_vulkan.h>

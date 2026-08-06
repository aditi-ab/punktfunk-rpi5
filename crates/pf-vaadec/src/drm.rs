//! The export descriptor — `vaExportSurfaceHandle`'s answer — and the walk that
//! turns it into the plane list a dmabuf import consumes.
//!
//! This is the one structure in the rung that the DRIVER writes and we read. Every
//! other buffer here is one we fill, where a wrong field is at worst refused; a
//! misread descriptor is plausible garbage — an fd taken from the middle of a
//! pitch, a plane count read out of a modifier's high word — and it imports
//! successfully into a texture of nonsense. So the layout is measured by
//! `layout-probe.c` like everything else, and the walk lives here, pure, where
//! macOS and the container run its tests.
//!
//! # The bug this walk exists to not repeat
//!
//! With `VA_EXPORT_SURFACE_SEPARATE_LAYERS` an NV12 surface comes back as **two
//! layers** — an `R8` luma layer and a `GR88` chroma layer, one plane each — not as
//! one two-plane `NV12` layer. Taking `layers[0]` and calling it the surface is how
//! this project once painted the screen green: the importer saw a single-plane R8
//! texture and the chroma was simply gone. Hence [`flatten`]: every plane of every
//! layer, in declared order, and the surface's format comes from the descriptor's
//! own top-level `fourcc` rather than from any layer's component format.
//!
//! (The alternative, `VA_EXPORT_SURFACE_COMPOSED_LAYERS`, asks the driver for one
//! layer describing the whole surface. It is not universally implemented, and the
//! separate-layers form is what the FFmpeg VAAPI path this rung replaces has always
//! used — so it is the form the fleet's drivers are exercised on.)

use std::os::raw::c_int;

/// `VA_EXPORT_SURFACE_READ_ONLY` — the decoder keeps writing this surface's future
/// siblings; the consumer only samples.
pub const VA_EXPORT_SURFACE_READ_ONLY: u32 = 0x0001;

/// `VA_EXPORT_SURFACE_SEPARATE_LAYERS` — one layer per plane (module docs).
pub const VA_EXPORT_SURFACE_SEPARATE_LAYERS: u32 = 0x0004;

/// `VA_FOURCC_NV12` — identical to `DRM_FORMAT_NV12`; the two namespaces agree on
/// the packed-fourcc value, which is why the descriptor's `fourcc` can be handed
/// to a DRM importer unchanged.
pub const VA_FOURCC_NV12: u32 = 0x3231_564e;

/// `VA_FOURCC_P010` — identical to `DRM_FORMAT_P010`.
pub const VA_FOURCC_P010: u32 = 0x3031_3050;

/// Fixed array bounds in the descriptor, measured (`layout-probe.c`).
pub const MAX_OBJECTS: usize = 4;
pub const MAX_LAYERS: usize = 4;
pub const MAX_PLANES_PER_LAYER: usize = 4;

/// One buffer object backing the surface: an fd we OWN and must close.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VaDrmPrimeObject {
    /// DRM PRIME fd. `c_int` because that is what the header says; the caller
    /// wraps it in an `OwnedFd` the moment the export succeeds.
    pub fd: c_int,
    pub size: u32,
    pub drm_format_modifier: u64,
}

/// One layer: under `SEPARATE_LAYERS` this is a single plane with its own
/// component format.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VaDrmPrimeLayer {
    /// The LAYER's DRM format (`R8`, `GR88`, …) — a component format, never the
    /// surface's. See the module docs.
    pub drm_format: u32,
    pub num_planes: u32,
    pub object_index: [u32; MAX_PLANES_PER_LAYER],
    pub offset: [u32; MAX_PLANES_PER_LAYER],
    pub pitch: [u32; MAX_PLANES_PER_LAYER],
}

/// `VADRMPRIMESurfaceDescriptor` (`va_drmcommon.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VaDrmPrimeSurfaceDescriptor {
    /// The SURFACE's fourcc (`VA_FOURCC_NV12`, `VA_FOURCC_P010`, …) — the combined
    /// format, and the one a DRM importer wants.
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub num_objects: u32,
    pub objects: [VaDrmPrimeObject; MAX_OBJECTS],
    pub num_layers: u32,
    pub layers: [VaDrmPrimeLayer; MAX_LAYERS],
}

impl VaDrmPrimeSurfaceDescriptor {
    /// A zeroed descriptor for the driver to fill.
    ///
    /// Zero is not a valid `num_objects`/`num_layers`, so a driver that returns
    /// success without writing anything is caught by [`flatten`] rather than read
    /// as a surface with no planes.
    pub fn zeroed() -> Self {
        Self {
            fourcc: 0,
            width: 0,
            height: 0,
            num_objects: 0,
            objects: [VaDrmPrimeObject {
                fd: -1,
                size: 0,
                drm_format_modifier: 0,
            }; MAX_OBJECTS],
            num_layers: 0,
            layers: [VaDrmPrimeLayer {
                drm_format: 0,
                num_planes: 0,
                object_index: [0; MAX_PLANES_PER_LAYER],
                offset: [0; MAX_PLANES_PER_LAYER],
                pitch: [0; MAX_PLANES_PER_LAYER],
            }; MAX_LAYERS],
        }
    }
}

/// One plane of the flattened surface. `fd` is BORROWED from the descriptor's
/// object list — several planes routinely name the same object — so the caller
/// owns the objects and the planes reference them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportedPlane {
    pub fd: c_int,
    pub offset: u32,
    pub stride: u32,
}

/// The flattened surface: what an importer needs, in plane order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedSurface {
    /// The combined DRM fourcc, from the descriptor's own top-level field.
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    /// The tiling modifier. Every object must agree on it — see [`flatten`].
    pub modifier: u64,
    /// Every plane of every layer, in declared order.
    pub planes: Vec<ExportedPlane>,
    /// The fds the caller OWNS and must close, one per object.
    pub object_fds: Vec<c_int>,
}

/// Why a descriptor cannot be read as a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportError {
    /// Zero (a driver that "succeeded" without writing) or more than the arrays hold.
    ObjectCount(u32),
    LayerCount(u32),
    PlaneCount {
        layer: usize,
        planes: u32,
    },
    /// A plane named an object outside `num_objects` — reading it would take an fd
    /// from uninitialised descriptor memory.
    ObjectIndex {
        layer: usize,
        plane: usize,
        index: u32,
    },
    /// An object came back without a usable fd.
    BadFd {
        object: usize,
        fd: c_int,
    },
    /// The objects disagree on the tiling modifier. A dmabuf import takes ONE
    /// modifier for the whole image, so importing plane 1 under plane 0's tiling
    /// would decode the chroma as if it were laid out some other way. Every fleet
    /// driver puts the whole surface in one BO; a driver that does not needs code
    /// that does not exist yet, and must say so rather than guess.
    MixedModifiers {
        first: u64,
        other: u64,
    },
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::ObjectCount(n) => {
                write!(
                    f,
                    "descriptor declares {n} objects (want 1..={MAX_OBJECTS})"
                )
            }
            ExportError::LayerCount(n) => {
                write!(f, "descriptor declares {n} layers (want 1..={MAX_LAYERS})")
            }
            ExportError::PlaneCount { layer, planes } => write!(
                f,
                "layer {layer} declares {planes} planes (want 1..={MAX_PLANES_PER_LAYER})"
            ),
            ExportError::ObjectIndex {
                layer,
                plane,
                index,
            } => write!(
                f,
                "layer {layer} plane {plane} names object {index}, which the descriptor \
                 does not have"
            ),
            ExportError::BadFd { object, fd } => {
                write!(f, "object {object} exported fd {fd}")
            }
            ExportError::MixedModifiers { first, other } => write!(
                f,
                "the surface's objects disagree on tiling ({first:#018x} vs {other:#018x}) — \
                 a single-modifier import cannot express it"
            ),
        }
    }
}

impl std::error::Error for ExportError {}

/// Flatten a descriptor into an importable surface: **every plane of every layer,
/// in declared order** (module docs).
///
/// Validates before it walks, so a malformed descriptor is a typed refusal and
/// never an out-of-bounds read of the fixed arrays. The caller owns
/// [`ExportedSurface::object_fds`] on success; on failure it owns the descriptor's
/// fds and must close them itself — this function takes no ownership either way,
/// because it cannot know whether the export call succeeded.
pub fn flatten(desc: &VaDrmPrimeSurfaceDescriptor) -> Result<ExportedSurface, ExportError> {
    let objects = desc.num_objects as usize;
    if objects == 0 || objects > MAX_OBJECTS {
        return Err(ExportError::ObjectCount(desc.num_objects));
    }
    let layers = desc.num_layers as usize;
    if layers == 0 || layers > MAX_LAYERS {
        return Err(ExportError::LayerCount(desc.num_layers));
    }
    for (i, o) in desc.objects[..objects].iter().enumerate() {
        if o.fd < 0 {
            return Err(ExportError::BadFd {
                object: i,
                fd: o.fd,
            });
        }
    }
    let modifier = desc.objects[0].drm_format_modifier;
    if let Some(o) = desc.objects[1..objects]
        .iter()
        .find(|o| o.drm_format_modifier != modifier)
    {
        return Err(ExportError::MixedModifiers {
            first: modifier,
            other: o.drm_format_modifier,
        });
    }

    let mut planes = Vec::with_capacity(layers * 2);
    for (l, layer) in desc.layers[..layers].iter().enumerate() {
        let n = layer.num_planes as usize;
        if n == 0 || n > MAX_PLANES_PER_LAYER {
            return Err(ExportError::PlaneCount {
                layer: l,
                planes: layer.num_planes,
            });
        }
        for p in 0..n {
            let index = layer.object_index[p];
            if index as usize >= objects {
                return Err(ExportError::ObjectIndex {
                    layer: l,
                    plane: p,
                    index,
                });
            }
            planes.push(ExportedPlane {
                fd: desc.objects[index as usize].fd,
                offset: layer.offset[p],
                stride: layer.pitch[p],
            });
        }
    }

    Ok(ExportedSurface {
        fourcc: desc.fourcc,
        width: desc.width,
        height: desc.height,
        modifier,
        planes,
        object_fds: desc.objects[..objects].iter().map(|o| o.fd).collect(),
    })
}

// Measured by `layout-probe.c` against libva 2.23.0 headers, not transcribed.
const _: () = {
    use std::mem::align_of;
    use std::mem::offset_of;
    use std::mem::size_of;
    assert!(size_of::<VaDrmPrimeObject>() == 16);
    assert!(size_of::<VaDrmPrimeLayer>() == 56);
    assert!(size_of::<VaDrmPrimeSurfaceDescriptor>() == 312);
    assert!(align_of::<VaDrmPrimeSurfaceDescriptor>() == 8);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, fourcc) == 0);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, width) == 4);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, height) == 8);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, num_objects) == 12);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, objects) == 16);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, num_layers) == 80);
    assert!(offset_of!(VaDrmPrimeSurfaceDescriptor, layers) == 84);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// `DRM_FORMAT_R8` / `DRM_FORMAT_GR88` — the component formats a driver reports
    /// per layer for NV12 under `SEPARATE_LAYERS`. Present only to build the
    /// realistic fixture; nothing in the walk reads them, which is the point.
    const DRM_FORMAT_R8: u32 = 0x2038_5220;
    const DRM_FORMAT_GR88: u32 = 0x3838_5247;
    const MOD: u64 = 0x0200_0000_0180_1002;

    /// What radeonsi/iHD actually hand back for an NV12 decode surface: ONE object,
    /// TWO layers of one plane each, chroma at a non-zero offset in the same buffer.
    fn nv12_two_layers() -> VaDrmPrimeSurfaceDescriptor {
        let mut d = VaDrmPrimeSurfaceDescriptor::zeroed();
        d.fourcc = VA_FOURCC_NV12;
        d.width = 1920;
        d.height = 1080;
        d.num_objects = 1;
        d.objects[0] = VaDrmPrimeObject {
            fd: 7,
            size: 1920 * 1088 * 3 / 2,
            drm_format_modifier: MOD,
        };
        d.num_layers = 2;
        d.layers[0] = VaDrmPrimeLayer {
            drm_format: DRM_FORMAT_R8,
            num_planes: 1,
            object_index: [0; 4],
            offset: [0; 4],
            pitch: [1920, 0, 0, 0],
        };
        d.layers[1] = VaDrmPrimeLayer {
            drm_format: DRM_FORMAT_GR88,
            num_planes: 1,
            object_index: [0; 4],
            offset: [1920 * 1088, 0, 0, 0],
            pitch: [1920, 0, 0, 0],
        };
        d
    }

    #[test]
    fn both_layers_become_planes_and_the_surface_keeps_its_own_fourcc() {
        let out = flatten(&nv12_two_layers()).expect("a well-formed NV12 export");
        // The green-screen regression, as an assertion: two planes, not one.
        assert_eq!(out.planes.len(), 2, "chroma was dropped");
        assert_eq!(
            out.fourcc, VA_FOURCC_NV12,
            "the surface fourcc must come from the descriptor, not from layers[0].drm_format \
             ({DRM_FORMAT_R8:#010x})"
        );
        assert_ne!(out.fourcc, DRM_FORMAT_R8);
        assert_eq!(out.planes[0].offset, 0);
        assert_eq!(out.planes[1].offset, 1920 * 1088);
        // Both planes live in the SAME object, so both carry the same fd — and the
        // caller must close it exactly once.
        assert_eq!(out.planes[0].fd, 7);
        assert_eq!(out.planes[1].fd, 7);
        assert_eq!(out.object_fds, vec![7]);
        assert_eq!(out.modifier, MOD);
    }

    #[test]
    fn a_multi_plane_layer_flattens_in_declared_order() {
        // The COMPOSED-ish shape: one layer that declares both planes itself. The
        // walk must handle it without caring which shape the driver chose.
        let mut d = VaDrmPrimeSurfaceDescriptor::zeroed();
        d.fourcc = VA_FOURCC_P010;
        d.num_objects = 2;
        d.objects[0] = VaDrmPrimeObject {
            fd: 11,
            size: 64,
            drm_format_modifier: MOD,
        };
        d.objects[1] = VaDrmPrimeObject {
            fd: 12,
            size: 32,
            drm_format_modifier: MOD,
        };
        d.num_layers = 1;
        d.layers[0] = VaDrmPrimeLayer {
            drm_format: VA_FOURCC_P010,
            num_planes: 2,
            object_index: [0, 1, 0, 0],
            offset: [0, 0, 0, 0],
            pitch: [3840, 3840, 0, 0],
        };
        let out = flatten(&d).expect("a well-formed two-object export");
        assert_eq!(out.planes.len(), 2);
        assert_eq!(out.planes[0].fd, 11);
        assert_eq!(out.planes[1].fd, 12);
        assert_eq!(out.object_fds, vec![11, 12], "both objects must be closed");
    }

    #[test]
    fn a_driver_that_wrote_nothing_is_refused_rather_than_read() {
        let d = VaDrmPrimeSurfaceDescriptor::zeroed();
        assert_eq!(flatten(&d), Err(ExportError::ObjectCount(0)));
    }

    #[test]
    fn counts_past_the_arrays_are_refused_before_the_walk() {
        let mut d = nv12_two_layers();
        d.num_objects = 5;
        assert_eq!(flatten(&d), Err(ExportError::ObjectCount(5)));
        let mut d = nv12_two_layers();
        d.num_layers = 9;
        assert_eq!(flatten(&d), Err(ExportError::LayerCount(9)));
        let mut d = nv12_two_layers();
        d.layers[1].num_planes = 5;
        assert_eq!(
            flatten(&d),
            Err(ExportError::PlaneCount {
                layer: 1,
                planes: 5
            })
        );
    }

    #[test]
    fn a_plane_naming_an_object_that_does_not_exist_is_refused() {
        // `num_objects` is 1, so object_index 1 addresses descriptor memory the
        // driver never wrote — an fd of -1 or worse, a stale one.
        let mut d = nv12_two_layers();
        d.layers[1].object_index[0] = 1;
        assert_eq!(
            flatten(&d),
            Err(ExportError::ObjectIndex {
                layer: 1,
                plane: 0,
                index: 1
            })
        );
    }

    #[test]
    fn objects_that_disagree_on_tiling_are_refused_not_averaged() {
        let mut d = nv12_two_layers();
        d.num_objects = 2;
        d.objects[1] = VaDrmPrimeObject {
            fd: 8,
            size: 16,
            drm_format_modifier: 0,
        };
        d.layers[1].object_index[0] = 1;
        assert_eq!(
            flatten(&d),
            Err(ExportError::MixedModifiers {
                first: MOD,
                other: 0
            })
        );
    }

    #[test]
    fn an_object_without_an_fd_is_refused() {
        let mut d = nv12_two_layers();
        d.objects[0].fd = -1;
        assert_eq!(flatten(&d), Err(ExportError::BadFd { object: 0, fd: -1 }));
    }
}

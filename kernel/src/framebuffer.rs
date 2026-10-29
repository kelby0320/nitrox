//! The system framebuffer as a userspace resource: Limine's aperture, captured at boot and handed
//! out as `/dev/framebuffer`. The kernel's own drawing is `crate::fbcon`, which reads Limine's
//! descriptor directly; this module is what userspace needs: where the pixels are, physically, and
//! how they are laid out.

use crate::libkern::framebuffer::FramebufferInfo;
use crate::libkern::lockrank::LockRank;
use crate::libkern::spinlock::IrqSpinLock;
use crate::limine::Framebuffer;
use crate::mm::{Caching, PAGE_SIZE, PhysAddr};

/// The system framebuffer's physical extent and pixel layout, captured once at boot.
///
/// Handing the aperture to userspace needs more than the console's virtual base and channel shifts:
/// the **physical** base, so a `MemoryObject` can describe the frames, and the channel *sizes*, so
/// a compositor can pack pixels for a layout that is not `0x00RRGGBB`. Captured because Limine's
/// descriptor lives in bootloader-reclaimable memory.
pub struct FramebufferAperture {
    /// Physical base address of the linear aperture.
    pub phys_base: PhysAddr,
    /// Geometry and channel layout, in the shape userspace receives.
    pub info: FramebufferInfo,
    /// How every mapping of it is cached.
    ///
    /// **Recorded here because two things need the same answer**: the `MemoryObject` minted when
    /// something resolves `/dev/framebuffer`, and the boot's measurement, which maps the aperture
    /// the way userspace's mapping is made. A measurement that named its own flags would go on
    /// saying "as userspace's is" after userspace's stopped being that.
    pub caching: Caching,
}

/// The captured aperture, or `None` if no usable framebuffer was reported. `Leaf` rank: taken
/// holding nothing, and nothing is taken under it; the read path copies the facts out at once.
static APERTURE: IrqSpinLock<Option<FramebufferAperture>> = IrqSpinLock::new(LockRank::Leaf, None);

/// Record Limine's framebuffer as a userspace-mappable aperture.
///
/// Refused, with the reason, when there is nothing usable: a depth other than 32 bits, an address
/// outside the HHDM, or a physical base inside a page, which a mapping of whole pages would put at
/// an offset no client is told of. Refusing is a boot-time diagnosis, where a client rendering
/// into a 16- or 24-bit buffer is an artefact to puzzle over later.
///
/// # Safety
///
/// `fb` must be Limine's live framebuffer descriptor, and `hhdm_offset` the offset
/// Limine reported, so that `address - hhdm_offset` is the aperture's physical base.
pub unsafe fn record_aperture(fb: &Framebuffer, hhdm_offset: u64, caching: Caching) -> Result<(), &'static str> {
    if fb.bpp != 32 {
        return Err("unsupported depth");
    }
    let virt = fb.address as u64;
    if virt < hhdm_offset {
        return Err("not an HHDM address");
    }
    let phys_base = virt - hhdm_offset;
    if !phys_base.is_multiple_of(PAGE_SIZE as u64) {
        return Err("base not page-aligned");
    }
    let byte_len = fb.pitch.saturating_mul(fb.height);
    let info = FramebufferInfo {
        width: fb.width as u32,
        height: fb.height as u32,
        pitch: fb.pitch,
        byte_len,
        bits_per_pixel: fb.bpp,
        red_shift: fb.red_mask_shift,
        red_size: fb.red_mask_size,
        green_shift: fb.green_mask_shift,
        green_size: fb.green_mask_size,
        blue_shift: fb.blue_mask_shift,
        blue_size: fb.blue_mask_size,
    };
    *APERTURE.lock() = Some(FramebufferAperture { phys_base: PhysAddr::new(phys_base), info, caching });
    Ok(())
}

/// The captured aperture's physical base, geometry and caching, if one was recorded.
pub fn aperture() -> Option<(PhysAddr, FramebufferInfo, Caching)> {
    APERTURE.lock().as_ref().map(|a| (a.phys_base, a.info, a.caching))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HHDM: u64 = 0xFFFF_8000_0000_0000;

    fn at(address: u64, bpp: u16) -> Framebuffer {
        Framebuffer {
            address: address as *mut u8,
            width: 1024,
            height: 768,
            pitch: 4096,
            bpp,
            memory_model: 1,
            red_mask_size: 8,
            red_mask_shift: 16,
            green_mask_size: 8,
            green_mask_shift: 8,
            blue_mask_size: 8,
            blue_mask_shift: 0,
            _unused: [0; 7],
            edid_size: 0,
            edid: core::ptr::null_mut(),
            mode_count: 0,
            modes: core::ptr::null_mut(),
        }
    }

    /// **Only a page-aligned base is recorded**: one inside a page would reach a client's mapping
    /// misaligned, and stop the kernel there with the address-space lock held.
    #[test]
    fn an_aperture_is_recorded_only_from_a_page_aligned_base_in_the_hhdm() {
        // SAFETY: only the descriptors' fields are read; nothing reaches the addresses they give.
        let record = |fb: Framebuffer| unsafe { record_aperture(&fb, HHDM, Caching::Normal) };
        assert_eq!(record(at(HHDM + 0x8000_0FFF, 32)), Err("base not page-aligned"));
        assert_eq!(record(at(HHDM + 0x8000_1001, 32)), Err("base not page-aligned"));
        assert_eq!(record(at(HHDM - PAGE_SIZE as u64, 32)), Err("not an HHDM address"));
        assert_eq!(record(at(HHDM + 0x8000_1000, 24)), Err("unsupported depth"));
        assert!(aperture().is_none());
        assert_eq!(record(at(HHDM + 0x8000_1000, 32)), Ok(()));
        assert_eq!(aperture().map(|(base, ..)| base), Some(PhysAddr::new(0x8000_1000)));
    }
}

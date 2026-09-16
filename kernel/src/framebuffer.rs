//! The system framebuffer as a userspace resource: Limine's aperture, captured at boot and
//! handed out as `/dev/framebuffer`.
//!
//! The kernel's own drawing — the console, from the first line of boot until a client takes the
//! screen, and again when the machine stops — is `crate::fbcon`, which reads Limine's descriptor
//! directly. This module is only what userspace needs: where the pixels are, physically, and how
//! they are laid out.

use crate::libkern::framebuffer::FramebufferInfo;
use crate::libkern::lockrank::LockRank;
use crate::libkern::spinlock::IrqSpinLock;
use crate::limine::Framebuffer;
use crate::mm::{Caching, PhysAddr};

/// The system framebuffer's physical extent and pixel layout, captured once at boot.
///
/// The console (`crate::fbcon`) draws through a *virtual* base (Limine reports an HHDM
/// address) and the three channel shifts. Handing the aperture to userspace needs strictly
/// more — the **physical** base, so a `MemoryObject` can describe the frames, and the channel
/// *sizes*, so a compositor can pack pixels for a layout that is not `0x00RRGGBB`.
///
/// Captured at boot rather than read on demand because Limine's descriptor lives in
/// bootloader-reclaimable memory: the facts must be copied out before anything reclaims
/// it.
pub struct FramebufferAperture {
    /// Physical base address of the linear aperture.
    pub phys_base: PhysAddr,
    /// Geometry and channel layout, in the shape userspace receives.
    pub info: FramebufferInfo,
    /// How every mapping of it is cached (Phase 5 Part G.3).
    ///
    /// **Recorded here because two things need the same answer**: the `MemoryObject` minted when
    /// something resolves `/dev/framebuffer`, and the boot's own measurement, which maps the
    /// aperture the way userspace's mapping is made and times a fill through it. A measurement
    /// that named its flags itself would go on saying "as userspace's is" after userspace's
    /// stopped being that — which is exactly what it did until a control caught it.
    pub caching: Caching,
}

/// The captured aperture, or `None` if no usable framebuffer was reported.
/// `Leaf` rank: taken while holding nothing, and nothing is taken while it is held —
/// the read path copies the facts out and releases immediately.
static APERTURE: IrqSpinLock<Option<FramebufferAperture>> =
    IrqSpinLock::new(LockRank::Leaf, None);

/// Record Limine's framebuffer as a userspace-mappable aperture.
///
/// Returns `false` when there is nothing usable to record — no framebuffer at all, or
/// a depth other than 32 bits. Refusing an unsupported depth is deliberate: it is a
/// boot-time diagnosis, where letting a client render into a 16- or 24-bit buffer is a
/// visual artefact to puzzle over later.
///
/// # Safety
///
/// `fb` must be Limine's live framebuffer descriptor, and `hhdm_offset` the offset
/// Limine reported, so that `address - hhdm_offset` is the aperture's physical base.
pub unsafe fn record_aperture(fb: &Framebuffer, hhdm_offset: u64, caching: Caching) -> bool {
    if fb.bpp != 32 {
        return false;
    }
    let virt = fb.address as u64;
    if virt < hhdm_offset {
        // Not an HHDM address; without a physical base there is nothing to hand out.
        return false;
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
    *APERTURE.lock() = Some(FramebufferAperture {
        phys_base: PhysAddr::new(virt - hhdm_offset),
        info,
        caching,
    });
    true
}

/// The captured aperture's physical base, geometry and caching, if one was recorded.
pub fn aperture() -> Option<(PhysAddr, FramebufferInfo, Caching)> {
    APERTURE.lock().as_ref().map(|a| (a.phys_base, a.info, a.caching))
}

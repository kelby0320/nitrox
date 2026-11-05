//! Compositing a client's buffer that the server reads by copying.

use alloc::vec;
use libdraw::format::{PixelFormat, Rgb};
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::pixels::Pixels;
use libos::{Handle, MapReadWrite, Mapping, Memory, mock};
use librsproto::surface::{AttachBufferRequest, CommitRequest, CreateWindowRequest, Role, SURFACE_FORMAT_XRGB8888};

use crate::{BufferSource, WindowStack};

/// One buffer on window 1, either borrowed whole or shared.
enum One<'a> {
    Own(&'a [u8]),
    Shared(&'a Mapping<MapReadWrite>),
}

impl BufferSource for One<'_> {
    fn pixels(&self, _window: u32, _buffer: u32) -> Option<Pixels<'_>> {
        Some(match self {
            One::Own(b) => Pixels::Own(b),
            One::Shared(m) => Pixels::Shared(*m),
        })
    }
}

fn stack_with_window() -> WindowStack {
    let mut s = WindowStack::new();
    let id = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).expect("create");
    s.mark_configured(id);
    let attach =
        AttachBufferRequest { window: id, buffer: 0, width: 8, height: 8, pitch: 32, format: SURFACE_FORMAT_XRGB8888 };
    s.attach(&attach).unwrap();
    s.commit(&CommitRequest { window: id, buffer: 0, damage_x: 0, damage_y: 0, damage_w: 0, damage_h: 0 }).unwrap();
    s
}

fn screen() -> MemFramebuffer {
    MemFramebuffer::new(Geometry::packed(32, 16, PixelFormat::XRGB8888))
}

fn striped() -> alloc::vec::Vec<u8> {
    let g = Geometry::packed(8, 8, PixelFormat::XRGB8888);
    let mut px = vec![0u8; g.byte_len()];
    for y in 0..8 {
        let word = g.format.encode(Rgb::new(0x20 + y as u8, 0x40, 0x80)).to_le_bytes();
        for x in 0..8 {
            let off = g.offset_of(x, y).unwrap();
            px[off..off + 4].copy_from_slice(&word);
        }
    }
    px
}

fn shared(bytes: &[u8]) -> Mapping<MapReadWrite> {
    mock::reset();
    let memory = Handle::<Memory, MapReadWrite>::create(bytes.len()).unwrap();
    let mut mapping = memory.map(bytes.len()).unwrap();
    mapping.write_from(0, bytes).unwrap();
    mapping
}

#[test]
fn a_shared_buffer_composes_as_the_same_bytes_borrowed_do() {
    let stack = stack_with_window();
    let px = striped();
    let mapping = shared(&px);
    let full = screen().geometry().bounds();

    let (mut own, mut copied) = (screen(), screen());
    stack.compose_into(&mut own, Rgb::new(9, 9, 9), &One::Own(&px), &[full]);
    stack.compose_into(&mut copied, Rgb::new(9, 9, 9), &One::Shared(&mapping), &[full]);

    assert_ne!(own.get_pixel(3, 3), Some(Rgb::new(9, 9, 9)), "the window drew nothing");
    assert_eq!(copied.bytes(), own.bytes());
}

//! The model's threads: one suspended on a fault is read and settled as the kernel does.

use super::*;

/// The model runs no thread to end, so a test that reaches this has gone wrong.
pub fn thread_exit(status: i32) -> ! {
    panic!("thread_exit({status})")
}

pub fn thread_get_registers(thread: u64, out: &mut RegisterValues) -> i64 {
    with(|m| match m.typed(thread, Rights::SIGNAL, KObjectType::Thread) {
        Ok((o, _)) => match m.objects[o] {
            Object::Thread(Some(regs)) => {
                *out = regs;
                0
            }
            _ => e(KError::InvalidArgument),
        },
        Err(r) => r,
    })
}

/// The disposition is checked before the handle, as the kernel checks it.
pub fn exception_resume(thread: u64, disposition: u64, _code: u64) -> i64 {
    if !matches!(disposition, 0 | 2) {
        return e(KError::Unsupported);
    }
    with(|m| match m.typed(thread, Rights::SIGNAL, KObjectType::Thread) {
        Ok((o, _)) if matches!(m.objects[o], Object::Thread(Some(_))) => {
            m.objects[o] = Object::Thread(None);
            0
        }
        Ok(_) => e(KError::InvalidArgument),
        Err(r) => r,
    })
}

/// Suspend thread `h` on a fault with `regs`, as a fault in it would.
pub fn fault_thread(h: u64, regs: RegisterValues) {
    with(|m| {
        let (o, _) = m.typed(h, Rights::empty(), KObjectType::Thread).expect("a thread handle");
        m.objects[o] = Object::Thread(Some(regs));
    })
}

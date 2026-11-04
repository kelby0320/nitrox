//! The model's clocks and timers.

use super::*;

pub fn clock_read(clock: u64, out: &mut u64) -> i64 {
    with(|m| match clock {
        libkern::CLOCK_MONOTONIC => {
            m.clock += 1_000;
            *out = m.clock;
            0
        }
        libkern::CLOCK_REALTIME => match m.realtime {
            Some(t) => {
                *out = t + m.clock;
                0
            }
            None => e(KError::Unsupported),
        },
        _ => e(KError::InvalidArgument),
    })
}

/// `InvalidArgument` outside 2000 to 2099, as the kernel answers, then what a test armed.
pub fn clock_set(ns: u64) -> i64 {
    const Y2000: u64 = 946_684_800_000_000_000;
    const Y2100: u64 = 4_102_444_800_000_000_000;
    with(|m| {
        if !(Y2000..Y2100).contains(&ns) {
            return e(KError::InvalidArgument);
        }
        let r = m.clock_answer.take().unwrap_or(libkern::CLOCK_SET_KEPT as i64);
        if r >= 0 {
            m.realtime = Some(ns.saturating_sub(m.clock));
        }
        r
    })
}

pub fn timer_create() -> i64 {
    with(|m| m.mint(Object::Timer(0), Rights::WAIT | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER))
}

/// One-shot only, as nothing in libos sets an interval.
pub fn timer_set(timer: u64, at: u64, interval: u64) -> i64 {
    assert_eq!(interval, 0, "a periodic timer the model does not have");
    with(|m| match m.typed(timer, Rights::empty(), KObjectType::Timer) {
        Ok((o, _)) => {
            m.objects[o] = Object::Timer(at);
            0
        }
        Err(r) => r,
    })
}

/// The soonest a set timer among `objs` fires by `deadline`: where an idle wait runs the clock to.
pub(super) fn next_fire(m: &Mock, objs: &[usize], deadline: u64) -> Option<u64> {
    let fires = objs.iter().filter_map(|&o| match m.objects[o] {
        Object::Timer(at) if at != 0 => Some(at),
        _ => None,
    });
    fires.min().filter(|&at| at <= deadline)
}

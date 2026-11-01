//! What a program in a view is started with, apart from the syscall that starts it.

use libkern::SpawnArgs;

/// The spawn block of a program in a view, moving it both ends it is handed.
pub fn program_args(image: u64, arg0: u64, setup: u64, life: u64, namespace: u64, syscaps: u64) -> SpawnArgs {
    SpawnArgs {
        image,
        handle_count: 2,
        move_mask: 0b11,
        arg0,
        // **The life channel**: a handle the program never learns about, so it stays open
        // until the program exits — which is how the broker learns *which* program it was
        // (`TODO(child-exit-attribution)`, as `service-mgr` does it).
        handles: [setup, life, 0, 0],
        // **No right on it**: one it could duplicate or move would outlive it.
        rights: [u64::MAX, 0, 0, 0],
        namespace,
        // No ambient authority but what a grant says.
        syscaps,
    }
}

/// A channel pair: this end, and the program's.
pub type Pair = (u64, u64);

/// **Both pairs or neither**: either create can fail alone, and the pair made goes to `close`.
pub fn both_pairs(setup: Option<Pair>, life: Option<Pair>, close: impl FnMut(u64)) -> Option<(Pair, Pair)> {
    if let (Some(setup), Some(life)) = (setup, life) {
        return Some((setup, life));
    }
    setup.into_iter().chain(life).flat_map(|(ours, child)| [ours, child]).for_each(close);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_holds_no_right_on_its_life_channel() {
        let args = program_args(1, 2, 3, 4, 5, 6);
        assert_eq!((args.handle_count, args.move_mask), (2, 0b11));
        assert_eq!(args.handles[..2], [3, 4]);
        assert_eq!(args.rights[..2], [u64::MAX, 0]);
    }

    #[test]
    fn a_pair_made_alone_is_closed() {
        let closed = |setup, life| {
            let mut closed = Vec::new();
            let both = both_pairs(setup, life, |h| closed.push(h));
            (both.is_some(), closed)
        };
        assert_eq!(closed(Some((1, 2)), Some((3, 4))), (true, vec![]));
        assert_eq!(closed(Some((1, 2)), None), (false, vec![1, 2]));
        assert_eq!(closed(None, Some((3, 4))), (false, vec![3, 4]));
        assert_eq!(closed(None, None), (false, vec![]));
    }
}

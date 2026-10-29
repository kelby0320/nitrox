//! The system calls a pipeline makes ([`nxsh::pipeline::Sys`]): the pipes, spawns and setup
//! messages that start this shell's stages, and the waits that end them.

use super::{
    NitroxHost, RIGHT_DUPLICATE, RIGHT_SEND, RIGHT_TRANSFER, SPAWN, SYS_HANDLE_CLOSE, SYS_HANDLE_DUPLICATE,
    SYS_PROCESS_SPAWN, SYS_PROCESS_TERMINATE, SYS_WAIT, bootstrap_arg0, drain_notifications, pipe, send_setup_full,
    stage, syscall1, syscall2, syscall4, tty_open_sibling,
};
use alloc::string::String;
use alloc::vec::Vec;
use libkern::abi::WAIT_RESULT_SIZE;
use libstream::setup::Streams;
use libstream::wire::Record;
use nxsh::pipeline::Sys;

/// What a pipeline's stages start from: the shell, their images and argvs, and one environment.
pub(crate) struct Stages<'a> {
    pub(crate) host: &'a mut NitroxHost,
    pub(crate) images: stage::Images,
    pub(crate) argvs: &'a [Vec<String>],
    pub(crate) env: &'a Record,
}

impl Sys for Stages<'_> {
    fn pipe(&mut self, depth: u32) -> Option<(u64, u64)> {
        pipe(depth).ok()
    }

    fn spawn(&mut self, stage: usize, setup: u64) -> Option<u64> {
        // SAFETY: `SPAWN` is our static, filled in immediately before use; this process is
        // single-threaded, so spawns are sequential.
        let proc = unsafe {
            SPAWN.image = self.images.0[stage];
            SPAWN.handles[0] = setup;
            SPAWN.arg0 = bootstrap_arg0(true);
            SPAWN.namespace = 0; // inherit a LOOKUP-only view of ours
            syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN) as u64)
        };
        self.images.release(stage);
        (proc >= 0).then_some(proc as u64)
    }

    fn duplicate(&mut self, sink: u64) -> Option<u64> {
        // With `DUPLICATE`, because a stage may hand its `stderr` on: `with` gives its program a
        // duplicate and keeps one to report on.
        // SAFETY: register-only syscall on a channel endpoint this shell owns.
        let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, sink, RIGHT_SEND | RIGHT_TRANSFER | RIGHT_DUPLICATE) };
        (dup > 0).then_some(dup as u64)
    }

    fn terminal(&mut self) -> Option<u64> {
        if self.host.tty != 0 { tty_open_sibling(self.host.tty) } else { None }
    }

    fn send_setup(&mut self, stage: usize, setup: u64, streams: Streams, terminal: Option<u64>) -> bool {
        let argv: Vec<&str> = self.argvs[stage].iter().map(|s| s.as_str()).collect();
        // The same environment to every stage, arguments as written.
        send_setup_full(setup, streams, terminal, &argv, self.env).is_ok()
    }

    fn say(&mut self, what: &str) {
        self.host.say(what);
    }

    fn close(&mut self, h: u64) {
        // SAFETY: closing a handle this shell holds.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }

    fn terminate(&mut self, child: u64) {
        // SAFETY: a Process handle this shell owns, with SIGNAL from spawn.
        unsafe { syscall1(SYS_PROCESS_TERMINATE, child) };
    }

    fn await_exits(&mut self, children: &[u64], exits: &mut Vec<(i32, bool)>) {
        let mut asked = true;
        loop {
            drain_notifications(self.host.notif, children, &mut asked, exits);
            if exits.len() >= children.len() {
                return;
            }
            let (handles, mut results) = ([self.host.notif], [0u8; WAIT_RESULT_SIZE]);
            // SAFETY: a one-entry handle array and room for its one result, both live for the call.
            let waited =
                unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
            if waited < 1 {
                return; // no exit can be read
            }
        }
    }
}

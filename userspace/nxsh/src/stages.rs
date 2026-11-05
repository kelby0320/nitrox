//! The system calls a pipeline makes ([`nxsh::pipeline::Sys`]): the pipes, spawns and setup
//! messages that start this shell's stages, and the waits that end them.
//!
//! The pipeline names its handles by their values; the handles themselves are held here until a
//! spawn or a send moves them or the pipeline closes them.

use super::NitroxHost;
use crate::stage;
use alloc::string::String;
use alloc::vec::Vec;
use libkern::{Rights, SysCaps};
use libos::{Channel, Deadline, File, Handle, MapRead, Only, Process, Ref, Spawn, WaitSet};
use libstream::setup::{StageStreams, Streams, bootstrap_arg0, send_stage_setup};
use libstream::wire::Record;
use nxsh::pipeline::Sys;

/// A handle a pipeline is using.
enum Held {
    Channel(Handle<Channel, Only>),
    Process(Handle<Process, Only>),
}

impl Held {
    fn key(&self) -> u64 {
        match self {
            Held::Channel(h) => h.raw().0,
            Held::Process(h) => h.raw().0,
        }
    }
}

/// What a pipeline's stages start from: the shell, their images and argvs, and one environment.
pub(crate) struct Stages<'a> {
    host: &'a mut NitroxHost,
    images: stage::Images,
    argvs: &'a [Vec<String>],
    env: &'a Record,
    held: Vec<Held>,
}

impl<'a> Stages<'a> {
    pub(crate) fn new(
        host: &'a mut NitroxHost,
        images: stage::Images,
        argvs: &'a [Vec<String>],
        env: &'a Record,
    ) -> Self {
        Stages { host, images, argvs, env, held: Vec::new() }
    }

    fn keep(&mut self, h: Handle<Channel, Only>) -> u64 {
        let key = h.raw().0;
        self.held.push(Held::Channel(h));
        key
    }

    /// The channel end `key` names, which the caller now owns.
    pub(crate) fn take_channel(&mut self, key: u64) -> Option<Handle<Channel, Only>> {
        let at = self.held.iter().position(|h| h.key() == key && matches!(h, Held::Channel(_)))?;
        match self.held.swap_remove(at) {
            Held::Channel(h) => Some(h),
            Held::Process(_) => None,
        }
    }

    /// The channel end `key` names, still held here.
    pub(crate) fn channel(&self, key: u64) -> Option<Ref<'_, Channel, Only>> {
        self.held.iter().find_map(|h| match h {
            Held::Channel(c) if c.raw().0 == key => Some(c.as_ref()),
            _ => None,
        })
    }
}

/// Start a child on `image`, handing it `theirs` as its bootstrap endpoint; a refused spawn gives
/// the endpoint back.
fn start(
    image: Option<Ref<'_, File, MapRead>>,
    theirs: Handle<Channel, Only>,
) -> Result<Handle<Process, Only>, Option<Handle<Channel, Only>>> {
    let Some(image) = image else { return Err(Some(theirs)) };
    let mut spawn = Spawn::new(image);
    // Whatever this shell holds, passed on; the kernel intersects it with its own (child = parent &
    // asked): nothing in a session, `SYSTEM_CLOCK` in a view with the `clock` grant, so `with admin
    // nxsh` has a working `date --set`. No namespace is named: the child inherits a LOOKUP-only view
    // of ours.
    spawn.arg0(bootstrap_arg0(true)).syscaps(SysCaps::all());
    if let Err(back) = spawn.grant(theirs, Rights::from_bits(u64::MAX)) {
        return Err(Some(back));
    }
    spawn.spawn().map_err(|mut refused| refused.moved.take(0))
}

impl Sys for Stages<'_> {
    fn pipe(&mut self, depth: u32) -> Option<(u64, u64)> {
        let (rx, tx) = Handle::<Channel, Only>::pair(depth).ok()?;
        Some((self.keep(rx), self.keep(tx)))
    }

    fn spawn(&mut self, stage: usize, setup: u64) -> Option<u64> {
        let theirs = self.take_channel(setup)?;
        let started = start(self.images.image(stage), theirs);
        self.images.release(stage);
        match started {
            Ok(child) => {
                let key = child.raw().0;
                self.held.push(Held::Process(child));
                Some(key)
            }
            Err(back) => {
                if let Some(h) = back {
                    self.keep(h);
                }
                None
            }
        }
    }

    fn duplicate(&mut self, sink: u64) -> Option<u64> {
        // With `DUPLICATE`, because a stage may hand its `stderr` on: `with` gives its program a
        // duplicate and keeps one to report on.
        let dup = self.channel(sink)?.duplicate(Rights::SEND | Rights::TRANSFER | Rights::DUPLICATE).ok()?;
        Some(self.keep(dup))
    }

    fn terminal(&mut self) -> Option<u64> {
        let h = self.host.tty.as_ref()?.open_sibling()?;
        Some(self.keep(h))
    }

    fn send_setup(&mut self, stage: usize, setup: u64, streams: Streams, terminal: Option<u64>) -> bool {
        let argv: Vec<&str> = self.argvs[stage].iter().map(|s| s.as_str()).collect();
        let mut own = |key: Option<u64>| key.and_then(|k| self.take_channel(k));
        let streams =
            StageStreams { stdin: own(streams.stdin), stdout: own(streams.stdout), stderr: own(streams.stderr) };
        let terminal = own(terminal);
        let Some(setup) = self.channel(setup) else { return false };
        // The same environment to every stage, arguments as written.
        send_stage_setup(setup, streams, terminal, &argv, self.env, Deadline::NEVER).is_ok()
    }

    fn say(&mut self, what: &str) {
        self.host.say(what);
    }

    fn close(&mut self, h: u64) {
        if let Some(at) = self.held.iter().position(|o| o.key() == h) {
            self.held.swap_remove(at);
        }
    }

    fn terminate(&mut self, child: u64) {
        for h in &self.held {
            if let Held::Process(p) = h
                && p.raw().0 == child
            {
                let _ = p.terminate();
            }
        }
    }

    fn await_exits(&mut self, children: &[u64], exits: &mut Vec<(i32, bool)>) {
        loop {
            // The pipeline is being stopped, so a terminate request read here has nothing to add.
            self.host.notices.drain(exits);
            if exits.len() >= children.len() {
                return;
            }
            let Some(notify) = self.host.notices.notify() else { return };
            let mut set = WaitSet::<1>::new();
            let _ = set.add(notify);
            // No exit can be read if the wait fails.
            if !matches!(set.wait(Deadline::NEVER), Ok(found) if !found.is_empty()) {
                return;
            }
        }
    }
}

//! **A pipeline's handles and stages, owned**: made through a [`Pipeline`] and held until handed
//! on, so on any return what the shell still holds is closed and every stage not reaped is stopped
//! and waited for. None is left blocked on a channel the shell forgot, and no exit is left for the
//! next pipeline's reap to count. The syscalls go through [`Sys`], so this is host-tested.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libstream::setup::Streams;

/// What a pipeline asks of the system.
pub trait Sys {
    /// A channel pair of `depth` messages: `(read end, write end)`.
    fn pipe(&mut self, depth: u32) -> Option<(u64, u64)>;
    /// Spawn stage `stage` with `setup` as its bootstrap endpoint: its process handle. A spawn that
    /// succeeds moves `setup`; one that fails leaves it here.
    fn spawn(&mut self, stage: usize, setup: u64) -> Option<u64>;
    /// A copy of the diagnostic sink `sink` for one stage.
    fn duplicate(&mut self, sink: u64) -> Option<u64>;
    /// A terminal for a stage, if the shell has one to give.
    fn terminal(&mut self) -> Option<u64>;
    /// Send stage `stage` its setup message on `setup`. **Every handle in it is gone on every
    /// return**, moved or closed (`libstream::setup::send_setup_full`).
    fn send_setup(&mut self, stage: usize, setup: u64, streams: Streams, terminal: Option<u64>) -> bool;
    /// Tell the person something.
    fn say(&mut self, what: &str);
    /// Close a handle the shell holds.
    fn close(&mut self, h: u64);
    /// Ask a stage to stop.
    fn terminate(&mut self, child: u64);
    /// Read exits into `exits` until it holds one for each of `children`, or none more can come.
    fn await_exits(&mut self, children: &[u64], exits: &mut Vec<(i32, bool)>);
}

/// The ends of a started pipeline the shell goes on using, each still the pipeline's to close.
#[derive(Clone, Copy, Debug)]
pub struct Ends {
    /// The head stage's `stdin`, when the pipeline is fed.
    pub feed: Option<u64>,
    /// The tail stage's output.
    pub capture: Option<u64>,
    /// The read end of every stage's shared `stderr`.
    pub diagnostics: u64,
}

/// A pipeline's handles and stages; see the module.
pub struct Pipeline<S: Sys> {
    /// The system it makes its calls through.
    pub sys: S,
    owned: Vec<u64>,
    children: Vec<u64>,
    exits: Vec<(i32, bool)>,
    reaped: bool,
}

impl<S: Sys> Pipeline<S> {
    /// A pipeline holding nothing yet.
    pub fn new(sys: S) -> Self {
        Pipeline { sys, owned: Vec::new(), children: Vec::new(), exits: Vec::new(), reaped: false }
    }

    /// The stages' process handles, and the exits read so far.
    pub fn parts(&mut self) -> (&[u64], &mut Vec<(i32, bool)>) {
        (&self.children, &mut self.exits)
    }

    /// Close `h`, which the pipeline holds.
    pub fn close(&mut self, h: u64) {
        if self.disown(h) {
            self.sys.close(h);
        }
    }

    /// Every stage's exit is read: the drop closes their handles and stops none.
    pub fn reaped(&mut self) {
        self.reaped = true;
    }

    fn disown(&mut self, h: u64) -> bool {
        let at = self.owned.iter().position(|&o| o == h);
        at.map(|i| self.owned.swap_remove(i)).is_some()
    }

    fn pipe(&mut self, depth: u32, what: &str) -> Result<(u64, u64), String> {
        let (rx, tx) = self.sys.pipe(depth).ok_or_else(|| format!("could not create {what}"))?;
        self.owned.extend([rx, tx]);
        Ok((rx, tx))
    }

    /// **Start a stage for each of `names`**, each spawned and sent its streams in turn, the first
    /// fed when `input`: the ends the shell goes on with.
    pub fn start(&mut self, names: &[&str], input: bool) -> Result<Ends, String> {
        let (mut upstream, mut feed) = (None, None);
        if input {
            let (rx, tx) = self.pipe(4, "an input pipe")?;
            (upstream, feed) = (Some(rx), Some(tx));
        }
        // One `stderr` for the whole pipeline (§1's shared diagnostic sink). The slack is 36 messages, and that is a bound: a stage sends with `SENDMODE_BLOCK`, so
        // once the ring and the kernel's pending-sender queue (`IpcChannel::MAX_PENDING_SENDS`, 4)
        // are full it blocks, at its 37th unread diagnostic. The shell must be draining by then; it
        // drains at every wait, and the one window left is named at the feed.
        let (diagnostics, sink) = self.pipe(32, "a diagnostic channel")?;
        let mut capture = None;
        for (i, name) in names.iter().enumerate() {
            let (rx, tx) = self.pipe(4, "a pipe")?;
            let (setup, theirs) = self.pipe(4, "a setup channel")?;
            let child = self.sys.spawn(i, theirs).ok_or_else(|| format!("could not spawn `{name}`"))?;
            self.disown(theirs);
            self.children.push(child);
            // A duplicate per stage, because a setup message moves its handles.
            let stderr = self.sys.duplicate(sink);
            if stderr.is_none() {
                // Said, since a stage without `stderr` falls back to `kprint`, which on a laptop
                // reaches nobody; still run, since its diagnostics are not worth refusing it.
                self.sys.say(&format!("nxsh: `{name}` gets no diagnostic channel; its errors go to the kernel log\n"));
            }
            let streams = Streams { stdin: upstream.take(), stdout: Some(tx), stderr };
            // A terminal of its own, a sibling on the same window, so a stage that must ask
            // something (`with`, for a password) asks where the person is typing.
            let terminal = self.sys.terminal();
            for h in streams.stdin.into_iter().chain([tx]) {
                self.disown(h);
            }
            if !self.sys.send_setup(i, setup, streams, terminal) {
                return Err(format!("could not hand `{name}` its streams"));
            }
            self.close(setup);
            if i + 1 == names.len() {
                capture = Some(rx);
            } else {
                upstream = Some(rx);
            }
        }
        // The shell's own write end goes before anything can block: the read end reports
        // end-of-stream only once every writer, the shell included, is gone.
        self.close(sink);
        Ok(Ends { feed, capture, diagnostics })
    }
}

impl<S: Sys> Drop for Pipeline<S> {
    fn drop(&mut self) {
        // What the shell held first: a stage blocked on one of them sees its peer gone, and exits.
        for h in core::mem::take(&mut self.owned) {
            self.sys.close(h);
        }
        if !self.reaped && self.exits.len() < self.children.len() {
            self.children.iter().for_each(|&c| self.sys.terminate(c));
            self.sys.await_exits(&self.children, &mut self.exits);
        }
        self.children.iter().for_each(|&c| self.sys.close(c));
    }
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;

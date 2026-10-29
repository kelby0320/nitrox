use super::*;

/// A system that fails one call when asked to, and records what became of every handle it made.
#[derive(Default)]
struct Fake {
    next: u64,
    made: Vec<u64>,
    closed: Vec<u64>,
    moved: Vec<u64>,
    children: Vec<u64>,
    terminated: Vec<u64>,
    awaited: bool,
    calls: usize,
    fail_at: Option<usize>,
}

impl Fake {
    fn failing(at: Option<usize>) -> Fake {
        Fake { fail_at: at, ..Fake::default() }
    }
    fn handle(&mut self) -> u64 {
        self.next += 1;
        self.made.push(self.next);
        self.next
    }
    fn fails(&mut self) -> bool {
        self.calls += 1;
        self.fail_at == Some(self.calls - 1)
    }
}

impl Sys for &mut Fake {
    fn pipe(&mut self, _depth: u32) -> Option<(u64, u64)> {
        (!self.fails()).then(|| (self.handle(), self.handle()))
    }
    fn spawn(&mut self, _stage: usize, setup: u64) -> Option<u64> {
        if self.fails() {
            return None;
        }
        self.moved.push(setup);
        let child = self.handle();
        self.children.push(child);
        Some(child)
    }
    fn duplicate(&mut self, _sink: u64) -> Option<u64> {
        (!self.fails()).then(|| self.handle())
    }
    fn terminal(&mut self) -> Option<u64> {
        (!self.fails()).then(|| self.handle())
    }
    fn send_setup(&mut self, _stage: usize, _setup: u64, streams: Streams, terminal: Option<u64>) -> bool {
        self.moved.extend(streams.ordered().into_iter().chain(terminal));
        !self.fails()
    }
    fn say(&mut self, _what: &str) {}
    fn close(&mut self, h: u64) {
        self.closed.push(h);
    }
    fn terminate(&mut self, child: u64) {
        self.terminated.push(child);
    }
    fn await_exits(&mut self, children: &[u64], exits: &mut Vec<(i32, bool)>) {
        self.awaited = true;
        exits.resize(children.len(), (0, false));
    }
}

/// Every handle `f` made was given back once, closed or moved, and each stage was stopped and its
/// exit waited for unless `reaped`.
fn given_back(f: &Fake, reaped: bool, tag: &str) {
    for h in &f.made {
        let closed = f.closed.iter().filter(|&c| c == h).count();
        let moved = f.moved.iter().filter(|&m| m == h).count();
        assert_eq!((closed, moved, closed + moved), (closed, moved, 1), "{tag}: handle {h}");
    }
    for c in &f.children {
        assert_eq!(f.terminated.contains(c), !reaped, "{tag}: stage {c} stopped");
    }
    assert_eq!(f.awaited, !reaped && !f.children.is_empty(), "{tag}: exits waited for");
}

const NAMES: [&str; 3] = ["a", "b", "c"];

/// **Each call of a start failing in turn leaves nothing behind**: every handle made is closed or
/// moved once, and every stage spawned is stopped and waited for. The old start leaked every pipe
/// from the first failing call on and left its stages blocked for ever.
#[test]
fn a_start_failing_at_any_call_gives_everything_back() {
    let mut all = Fake::failing(None);
    Pipeline::new(&mut all).start(&NAMES, true).unwrap();
    assert_eq!(all.calls, 2 + 6 * NAMES.len(), "two pipes, then six calls a stage");
    for at in 0..all.calls {
        let mut f = Fake::failing(Some(at));
        let started = Pipeline::new(&mut f).start(&NAMES, true).is_ok();
        given_back(&f, false, &format!("call {at} failing, started {started}"));
    }
}

/// **A pipeline left once started**, as at an output past what the shell holds, closes its ends and
/// stops and waits for its stages; reaped, it closes their handles and stops none.
#[test]
fn a_started_pipeline_gives_everything_back_left_or_reaped() {
    let mut f = Fake::failing(None);
    let mut p = Pipeline::new(&mut f);
    let ends = p.start(&NAMES, true).unwrap();
    assert!(ends.feed.is_some() && ends.capture.is_some());
    drop(p);
    given_back(&f, false, "left");

    let mut f = Fake::failing(None);
    let mut p = Pipeline::new(&mut f);
    let ends = p.start(&NAMES, false).unwrap();
    assert_eq!(ends.feed, None);
    for h in [ends.capture.unwrap(), ends.diagnostics] {
        p.close(h);
    }
    let (children, exits) = p.parts();
    exits.resize(children.len(), (0, false));
    p.reaped();
    drop(p);
    given_back(&f, true, "reaped");
}

/// **A started pipeline holds only the ends it hands back, and its stages**: each setup end and its
/// own copy of the diagnostic sink are closed as it starts, or the sink's read end would never see
/// its last writer go.
#[test]
fn a_start_keeps_only_its_ends_and_stages() {
    let mut f = Fake::failing(None);
    let mut p = Pipeline::new(&mut f);
    let ends = p.start(&NAMES, true).unwrap();
    let fake = &p.sys;
    let open = |h: &&u64| !fake.closed.contains(h) && !fake.moved.contains(h);
    let mut held: Vec<u64> = fake.made.iter().filter(open).copied().collect();
    let mut want = vec![ends.feed.unwrap(), ends.capture.unwrap(), ends.diagnostics];
    want.extend(&fake.children);
    held.sort();
    want.sort();
    assert_eq!(held, want);
}

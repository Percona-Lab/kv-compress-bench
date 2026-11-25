//! One worker thread: an epoll (mio) loop driving many connections.
//!
//! Closed loop: each connection keeps `depth` requests in flight and sends the
//! next one as soon as a reply arrives.
//! Open loop: requests are scheduled at fixed (or Poisson) intervals; latency
//! is measured from the *scheduled* time, so a stalled server shows up as
//! latency instead of silently lowering the request rate (no coordinated
//! omission). Service time (from the moment the request is written) is kept as
//! a separate histogram.

use crate::codec::{Codec, Kind};
use crate::dist::{KeyDist, Rng, Scramble};
use crate::resp::{self, Reply};
use crate::values::ValueGen;
use hdrhistogram::Histogram;
use mio::net::TcpStream;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use std::collections::VecDeque;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub const HIST_MAX_NS: u64 = 60_000_000_000;

pub fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub fn new_hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, HIST_MAX_NS, 3).unwrap()
}

#[derive(Clone)]
pub enum Mode {
    /// SET every key in [start, end) once (version 0).
    Load,
    Run,
}

pub struct Shared {
    pub addrs: Vec<std::net::SocketAddr>,
    pub mode: Mode,
    pub conns: usize,
    pub depth: usize,
    /// Requests per second per thread (open loop) or None (closed loop).
    pub rate_per_thread: Option<f64>,
    pub poisson: bool,
    pub max_pending: usize,
    pub get_ratio: f64,
    pub keys: u64,
    pub dist: KeyDist,
    pub scramble: Option<Scramble>,
    pub prefix: Vec<u8>,
    pub vals: Arc<ValueGen>,
    pub verify: bool,
    /// compress values in the client before SET, decompress after GET
    pub client_compress: Option<Kind>,
    /// --check: per key, a 32-bit tag of the version the last SET sent (0 = not known yet). Each key is
    /// owned by one connection, so a GET must return exactly the last SET sent before it on that connection.
    pub check: Option<Vec<AtomicU32>>,
    pub threads: usize,
    pub stop: AtomicBool,
    pub start_ns: AtomicU64,
    pub interval_ns: u64,
    pub load_next: AtomicU64,
    pub load_chunk: u64,
    /// request-stream seed (load: the data seed)
    pub seed: u64,
    pub spin_ns: u64,
}

pub struct Snapshot {
    pub idx: u64,
    pub thread: usize,
    pub get_rt: Histogram<u64>,
    pub set_rt: Histogram<u64>,
    pub get_svc: Histogram<u64>,
    pub set_svc: Histogram<u64>,
    /// open loop: how late requests were written relative to their schedule
    pub send_lag: Histogram<u64>,
    pub misses: u64,
    pub errors: u64,
    pub verify_fail: u64,
    pub backlog: u64,
    pub final_: bool,
    /// client compression: value bytes before / after compression (SETs), sent with the final snapshot
    pub value_raw_bytes: u64,
    pub value_comp_bytes: u64,
    /// --check counters: GETs compared with the expected version, mismatches, versions learned (first read)
    pub check_ok: u64,
    pub check_fail: u64,
    pub check_learned: u64,
}

impl Snapshot {
    fn new(idx: u64, thread: usize) -> Self {
        Snapshot {
            idx,
            thread,
            get_rt: new_hist(),
            set_rt: new_hist(),
            get_svc: new_hist(),
            set_svc: new_hist(),
            send_lag: new_hist(),
            misses: 0,
            errors: 0,
            verify_fail: 0,
            backlog: 0,
            final_: false,
            value_raw_bytes: 0,
            value_comp_bytes: 0,
            check_ok: 0,
            check_fail: 0,
            check_learned: 0,
        }
    }
}

const OP_GET: u8 = 0;
const OP_SET: u8 = 1;

struct Pending {
    op: u8,
    key: u64,
    intended: u64,
    sent: u64,
    /// --check: expected version tag when the GET was sent (0 = unknown)
    exp: u32,
}

/// 32-bit tag of a value version (never 0, which means "unknown")
#[inline]
pub fn ver_tag(ver: u64) -> u32 {
    (crate::dist::mix64(ver) as u32) | 1
}

struct Conn {
    stream: TcpStream,
    fd: i32,
    wbuf: Vec<u8>,
    wpos: usize,
    rbuf: Vec<u8>,
    rpos: usize,
    inflight: VecDeque<Pending>,
    dirty: bool,
    want_write: bool,
    token: Token,
}

impl Conn {
    fn flush(&mut self) -> io::Result<()> {
        while self.wpos < self.wbuf.len() {
            match self.stream.write(&self.wbuf[self.wpos..]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write 0")),
                Ok(n) => self.wpos += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if self.wpos == self.wbuf.len() {
            self.wbuf.clear();
            self.wpos = 0;
        }
        Ok(())
    }

    /// Read everything available. Returns false on EOF.
    fn fill(&mut self) -> io::Result<bool> {
        loop {
            if self.rpos > 0 && self.rpos == self.rbuf.len() {
                self.rbuf.clear();
                self.rpos = 0;
            } else if self.rpos > 0 && self.rpos > self.rbuf.capacity() / 2 {
                self.rbuf.copy_within(self.rpos.., 0);
                let l = self.rbuf.len() - self.rpos;
                self.rbuf.truncate(l);
                self.rpos = 0;
            }
            if self.rbuf.capacity() - self.rbuf.len() < 16384 {
                self.rbuf.reserve(65536);
            }
            let len = self.rbuf.len();
            let spare = self.rbuf.capacity() - len;
            let n = unsafe { libc::read(self.fd, self.rbuf.as_mut_ptr().add(len) as *mut libc::c_void, spare) };
            if n > 0 {
                unsafe { self.rbuf.set_len(len + n as usize) };
                if (n as usize) < spare {
                    return Ok(true);
                }
            } else if n == 0 {
                return Ok(false);
            } else {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::WouldBlock => return Ok(true),
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(e),
                }
            }
        }
    }
}

pub fn pin_to_cpu(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

struct Worker<'a> {
    sh: &'a Shared,
    tid: usize,
    rng: Rng,
    conns: Vec<Conn>,
    snap: Snapshot,
    ver_counter: u64,
    load_cur: u64,
    load_end: u64,
    errors_printed: u32,
    codec: Option<Codec>,
}

impl<'a> Worker<'a> {
    #[inline]
    fn next_key(&mut self) -> Option<u64> {
        match self.sh.mode {
            Mode::Load => {
                if self.load_cur >= self.load_end {
                    let s = self.sh.load_next.fetch_add(self.sh.load_chunk, Ordering::Relaxed);
                    if s >= self.sh.keys {
                        return None;
                    }
                    self.load_cur = s;
                    self.load_end = (s + self.sh.load_chunk).min(self.sh.keys);
                }
                self.load_cur += 1;
                Some(self.load_cur - 1)
            }
            Mode::Run => {
                let r = self.sh.dist.sample(&mut self.rng, self.sh.keys);
                Some(match &self.sh.scramble {
                    Some(s) => s.apply(r, self.sh.keys),
                    None => r,
                })
            }
        }
    }

    /// Queue one request on connection `ci`. Returns false if no key is left (load).
    #[inline]
    fn issue(&mut self, ci: usize, intended: u64, now: u64) -> bool {
        let mut key = match self.next_key() {
            Some(k) => k,
            None => return false,
        };
        if self.sh.check.is_some() && matches!(self.sh.mode, Mode::Run) {
            // move the key to the residue class owned by (this thread, this connection), keeping its neighbourhood
            let unit = (self.sh.threads * self.sh.conns) as u64;
            let slot = (ci * self.sh.threads + self.tid) as u64;
            key = (key / unit) * unit + slot;
            if key >= self.sh.keys {
                key -= unit;
            }
        }
        let op = match self.sh.mode {
            Mode::Load => OP_SET,
            Mode::Run => {
                if self.rng.f64() < self.sh.get_ratio { OP_GET } else { OP_SET }
            }
        };
        let mut exp = 0u32;
        let c = &mut self.conns[ci];
        if op == OP_GET {
            if let Some(chk) = &self.sh.check {
                exp = chk[key as usize].load(Ordering::Relaxed);
            }
            resp::encode_get(&mut c.wbuf, &self.sh.prefix, key);
        } else {
            let ver = match self.sh.mode {
                Mode::Load => 0,
                Mode::Run => {
                    self.ver_counter += 1;
                    ((self.tid as u64 + 1) << 40) | (self.sh.seed & 0xFF) << 32 /* differs between runs: SET versions stay unique */ | (self.ver_counter & 0xFFFF_FFFF)
                }
            };
            if let Some(chk) = &self.sh.check {
                chk[key as usize].store(ver_tag(ver), Ordering::Relaxed);
            }
            if let Some(cd) = self.codec.as_mut() {
                cd.raw.clear();
                self.sh.vals.write(key, ver, &mut cd.raw);
                cd.compress();
                resp::encode_set_bytes(&mut c.wbuf, &self.sh.prefix, key, &cd.out);
            } else {
                resp::encode_set(&mut c.wbuf, &self.sh.prefix, key, ver, &self.sh.vals);
            }
        }
        if self.sh.rate_per_thread.is_some() {
            self.snap.send_lag.saturating_record(now.saturating_sub(intended).max(1));
        }
        c.inflight.push_back(Pending { op, key, intended, sent: now, exp });
        c.dirty = true;
        true
    }

    /// Parse all complete replies on connection `ci`. Returns the number parsed.
    fn drain_replies(&mut self, ci: usize, now: u64) -> Result<usize, String> {
        let mut done = 0;
        loop {
            let c = &mut self.conns[ci];
            let (reply, used) = match resp::parse(&c.rbuf, c.rpos)? {
                Some(x) => x,
                None => break,
            };
            c.rpos += used;
            let p = match c.inflight.pop_front() {
                Some(p) => p,
                None => return Err("reply without a request".into()),
            };
            let rt = now.saturating_sub(p.intended).max(1);
            let svc = now.saturating_sub(p.sent).max(1);
            match (p.op, reply) {
                (OP_GET, Reply::Bulk(s, l)) => {
                    let mut ok_bytes = true;
                    if let Some(cd) = self.codec.as_mut() {
                        // decompress every value (that is the client's cost), check it if asked
                        if !cd.decompress(&c.rbuf[s..s + l]) {
                            self.snap.verify_fail += 1;
                            ok_bytes = false;
                        } else if self.sh.verify && !self.sh.vals.verify(p.key, &cd.raw) {
                            self.snap.verify_fail += 1;
                            ok_bytes = false;
                        }
                    } else if self.sh.verify && !self.sh.vals.verify(p.key, &c.rbuf[s..s + l]) {
                        self.snap.verify_fail += 1;
                        ok_bytes = false;
                    }
                    if let Some(chk) = &self.sh.check {
                        let body: &[u8] = match self.codec.as_ref() { Some(cd) => &cd.raw, None => &c.rbuf[s..s + l] };
                        match (ok_bytes, self.sh.vals.header_version(p.key, body)) {
                            (true, Some(v)) => {
                                let t = ver_tag(v);
                                if p.exp == 0 {
                                    // first read of this key in this run: learn it, unless a SET was sent since
                                    let _ = chk[p.key as usize].compare_exchange(0, t, Ordering::Relaxed, Ordering::Relaxed);
                                    self.snap.check_learned += 1;
                                } else if t == p.exp {
                                    self.snap.check_ok += 1;
                                } else {
                                    self.snap.check_fail += 1;
                                    if self.errors_printed < 10 {
                                        self.errors_printed += 1;
                                        eprintln!("CHECK FAIL key {}: got version {:x}, expected tag {:x}", p.key, v, p.exp);
                                    }
                                }
                            }
                            _ => self.snap.check_fail += 1,
                        }
                    }
                }
                (OP_GET, Reply::Nil) if self.sh.check.is_some() => {
                    // every key was loaded: a miss is a lost key
                    self.snap.misses += 1;
                    self.snap.check_fail += 1;
                    if self.errors_printed < 10 {
                        self.errors_printed += 1;
                        eprintln!("CHECK FAIL key {}: GET returned nil", p.key);
                    }
                }
                (OP_GET, Reply::Nil) => self.snap.misses += 1,
                (OP_SET, Reply::Ok) => {}
                (_, Reply::Error) => {
                    self.snap.errors += 1;
                    if self.errors_printed < 5 {
                        self.errors_printed += 1;
                        eprintln!("server error reply (op {}, key {})", if p.op == OP_GET { "GET" } else { "SET" }, p.key);
                    }
                }
                (_, r) => {
                    self.snap.errors += 1;
                    if self.errors_printed < 5 {
                        self.errors_printed += 1;
                        eprintln!("unexpected reply {:?}", r);
                    }
                }
            }
            if p.op == OP_GET {
                self.snap.get_rt.saturating_record(rt);
                self.snap.get_svc.saturating_record(svc);
            } else {
                self.snap.set_rt.saturating_record(rt);
                self.snap.set_svc.saturating_record(svc);
            }
            done += 1;
        }
        Ok(done)
    }
}

pub fn run_worker(sh: Arc<Shared>, tid: usize, cpu: Option<usize>, tx: Sender<Snapshot>) -> Result<(), String> {
    if let Some(c) = cpu {
        pin_to_cpu(c);
    }
    unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1 as libc::c_ulong) };
    let sh: &Shared = &sh;
    let mut poll = Poll::new().map_err(|e| e.to_string())?;
    let mut conns = Vec::with_capacity(sh.conns);
    for i in 0..sh.conns {
        let addr = sh.addrs[(tid * sh.conns + i) % sh.addrs.len()];
        let s = std::net::TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
        s.set_nodelay(true).ok();
        s.set_nonblocking(true).map_err(|e| e.to_string())?;
        let mut stream = TcpStream::from_std(s);
        poll.registry()
            .register(&mut stream, Token(i), Interest::READABLE)
            .map_err(|e| e.to_string())?;
        let fd = stream.as_raw_fd();
        conns.push(Conn {
            stream,
            fd,
            wbuf: Vec::with_capacity(64 * 1024),
            wpos: 0,
            rbuf: Vec::with_capacity(128 * 1024),
            rpos: 0,
            inflight: VecDeque::with_capacity(sh.depth.max(16)),
            dirty: false,
            want_write: false,
            token: Token(i),
        });
    }
    // timerfd for precise open-loop wake-ups
    let tfd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC) };
    if tfd < 0 {
        return Err("timerfd_create failed".into());
    }
    const TIMER: Token = Token(usize::MAX - 1);
    poll.registry().register(&mut SourceFd(&tfd), TIMER, Interest::READABLE).map_err(|e| e.to_string())?;

    // wait for the common start time
    while sh.start_ns.load(Ordering::Acquire) == 0 {
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
    let start = sh.start_ns.load(Ordering::Acquire);
    while now_ns() < start {
        std::hint::spin_loop();
    }

    let mut w = Worker {
        sh,
        tid,
        rng: Rng::new(sh.seed ^ ((tid as u64 + 1) * 0x9E37_79B9)),
        conns,
        snap: Snapshot::new(0, tid),
        ver_counter: 0,
        load_cur: 0,
        load_end: 0,
        errors_printed: 0,
        codec: sh.client_compress.map(|k| Codec::new(k, sh.vals.size.max())),
    };
    let nconns = w.conns.len();
    let open = sh.rate_per_thread;
    let mut next_report = start + sh.interval_ns;
    let mut idx = 0u64;
    let mut exhausted = false;
    let mut stopping = false;
    let mut drain_deadline = u64::MAX;

    // open-loop schedule
    let mean_gap = open.map(|r| 1e9 / r).unwrap_or(0.0);
    // stagger threads so they do not fire in lockstep
    let mut t_next = start as f64 + if open.is_some() { mean_gap * (tid as f64 + 0.5) / 64.0 } else { 0.0 };
    let mut backlog: VecDeque<u64> = VecDeque::new();
    let mut rr = 0usize;

    // closed loop: fill the pipelines
    if open.is_none() {
        let now = now_ns();
        'fill: for ci in 0..nconns {
            for _ in 0..sh.depth {
                if !w.issue(ci, now, now) {
                    exhausted = true;
                    break 'fill;
                }
            }
        }
    }

    let mut events = Events::with_capacity(1024);
    let mut armed_for = 0u64;
    loop {
        let now = now_ns();
        // --- reporting boundaries
        while now >= next_report {
            let mut s = std::mem::replace(&mut w.snap, Snapshot::new(idx + 1, tid));
            s.idx = idx;
            s.backlog = backlog.len() as u64;
            tx.send(s).ok();
            idx += 1;
            next_report += sh.interval_ns;
        }
        if !stopping && sh.stop.load(Ordering::Relaxed) {
            stopping = true;
            drain_deadline = now + 2_000_000_000;
        }
        let inflight: usize = w.conns.iter().map(|c| c.inflight.len()).sum();
        if (stopping || exhausted) && inflight == 0 {
            break;
        }
        if now > drain_deadline {
            break;
        }

        // --- open loop: schedule due requests
        if let (Some(_), false) = (open, stopping) {
            while t_next <= now as f64 {
                backlog.push_back(t_next as u64);
                t_next += if sh.poisson { mean_gap * w.rng.exp1() } else { mean_gap };
            }
            let mut tries = 0;
            while let Some(&intended) = backlog.front() {
                if tries >= nconns {
                    break;
                }
                let ci = rr;
                rr = (rr + 1) % nconns;
                if w.conns[ci].inflight.len() < sh.max_pending {
                    backlog.pop_front();
                    w.issue(ci, intended, now);
                    tries = 0;
                } else {
                    tries += 1;
                }
            }
        }

        // --- flush writes
        for c in w.conns.iter_mut() {
            if c.dirty {
                c.flush().map_err(|e| format!("write: {e}"))?;
                c.dirty = false;
                let blocked = !c.wbuf.is_empty();
                if blocked != c.want_write {
                    let int = if blocked { Interest::READABLE | Interest::WRITABLE } else { Interest::READABLE };
                    poll.registry().reregister(&mut c.stream, c.token, int).map_err(|e| e.to_string())?;
                    c.want_write = blocked;
                }
            }
        }

        // --- wait
        let timeout = if stopping {
            Some(std::time::Duration::from_millis(10))
        } else if open.is_some() {
            let wait = (t_next as u64).saturating_sub(now_ns());
            if wait <= sh.spin_ns {
                Some(std::time::Duration::ZERO)
            } else {
                let target = t_next as u64;
                if armed_for != target {
                    let its = libc::itimerspec {
                        it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
                        it_value: libc::timespec { tv_sec: (target / 1_000_000_000) as i64, tv_nsec: (target % 1_000_000_000) as i64 },
                    };
                    unsafe { libc::timerfd_settime(tfd, libc::TFD_TIMER_ABSTIME, &its, std::ptr::null_mut()) };
                    armed_for = target;
                }
                Some(std::time::Duration::from_millis(100))
            }
        } else {
            Some(std::time::Duration::from_millis(100))
        };
        match poll.poll(&mut events, timeout) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        }
        let now = now_ns();
        for ev in events.iter() {
            let tok = ev.token();
            if tok == TIMER {
                let mut b = [0u8; 8];
                unsafe { libc::read(tfd, b.as_mut_ptr() as *mut libc::c_void, 8) };
                armed_for = 0;
                continue;
            }
            let ci = tok.0;
            if ev.is_writable() {
                let c = &mut w.conns[ci];
                c.flush().map_err(|e| format!("write: {e}"))?;
                c.dirty = true; // re-evaluate write interest in the flush pass
            }
            if ev.is_readable() || ev.is_read_closed() {
                let alive = w.conns[ci].fill().map_err(|e| format!("read: {e}"))?;
                let n = w.drain_replies(ci, now)?;
                if !alive {
                    return Err("server closed the connection".into());
                }
                // closed loop: replace every completed request
                if open.is_none() && !stopping && !exhausted {
                    for _ in 0..n {
                        if !w.issue(ci, now, now) {
                            exhausted = true;
                            break;
                        }
                    }
                }
            }
        }
    }
    let mut s = std::mem::replace(&mut w.snap, Snapshot::new(idx, tid));
    s.idx = idx;
    s.backlog = backlog.len() as u64;
    s.final_ = true;
    if let Some(cd) = w.codec.as_ref() {
        s.value_raw_bytes = cd.raw_bytes;
        s.value_comp_bytes = cd.comp_bytes;
    }
    tx.send(s).ok();
    unsafe { libc::close(tfd) };
    Ok(())
}

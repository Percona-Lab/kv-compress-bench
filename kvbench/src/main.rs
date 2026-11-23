//! kvbench: low-overhead benchmark for Redis-compatible key-value stores.

mod dist;
mod metrics;
mod resp;
mod values;
mod worker;

use clap::{Args, Parser, Subcommand, ValueEnum};
use dist::{KeyDist, Scramble, SizeDist};
use hdrhistogram::Histogram;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use values::ValueGen;
use worker::{new_hist, now_ns, Mode, Shared, Snapshot};

#[derive(Parser)]
#[command(name = "kvbench", version, about = "Benchmark for Redis-compatible key-value stores (GET/SET on integer keys)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// SET every key 0..keys-1 once (version 0).
    Load(LoadArgs),
    /// Run the GET/SET workload.
    Run(RunArgs),
    /// Print the compressibility of the generated values (no server needed).
    Ratio(RatioArgs),
}

#[derive(Args, Clone)]
struct DataArgs {
    /// Number of keys (integers 0..keys-1).
    #[arg(long, default_value_t = 1_000_000)]
    keys: u64,
    /// Optional key prefix, e.g. "k:" -> "k:123".
    #[arg(long, default_value = "")]
    key_prefix: String,
    /// Value size: fixed:N | uniform:MIN:MAX | normal:MEAN:STDDEV[:MIN:MAX] | lognormal:MEDIAN:SIGMA[:MIN:MAX]
    #[arg(long, default_value = "fixed:1024")]
    value_size: String,
    /// Percent of value characters that are random (rest is fixed filler text).
    /// 100 = random characters only (lz4 ~1.0x); 10 = lz4 ~3x.
    #[arg(long, default_value_t = 100.0)]
    value_random_pct: f64,
    /// Size of the shared value pool in MiB.
    #[arg(long, default_value_t = 64)]
    pool_mb: usize,
    /// Seed for all generators.
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

#[derive(Args)]
struct RatioArgs {
    #[command(flatten)]
    data: DataArgs,
    /// Extra zstd levels to report, comma separated (negative = fast levels), e.g. "-5,-1,1,3"
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    zstd_levels: String,
    /// Number of 4 KiB pages to sample.
    #[arg(long, default_value_t = 2048)]
    pages: usize,
    /// Also write the sampled pages to this file.
    #[arg(long)]
    dump: Option<String>,
}

#[derive(Args, Clone)]
struct ConnArgs {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port(s), comma separated; connections are spread over them.
    #[arg(long, default_value = "6379")]
    port: String,
    /// Worker threads.
    #[arg(long, default_value_t = 4)]
    threads: usize,
    /// Connections per thread.
    #[arg(long, default_value_t = 8)]
    conns: usize,
    /// Pin worker threads to these CPUs, e.g. "6-11" or "6,7,8".
    #[arg(long)]
    cpus: Option<String>,
    /// Server PID, for server CPU / RSS / swap from /proc.
    #[arg(long)]
    server_pid: Option<u32>,
    /// Reporting interval in seconds.
    #[arg(long, default_value_t = 1.0)]
    interval: f64,
    /// Append JSON lines (intervals + summary) to this file.
    #[arg(long)]
    out: Option<String>,
    /// Free-form label stored in the output.
    #[arg(long, default_value = "")]
    label: String,
    /// Do not print per-interval lines.
    #[arg(long)]
    quiet: bool,
}

#[derive(Args)]
struct LoadArgs {
    #[command(flatten)]
    data: DataArgs,
    #[command(flatten)]
    conn: ConnArgs,
    /// Requests in flight per connection during the load.
    #[arg(long, default_value_t = 64)]
    pipeline: usize,
    /// Run FLUSHALL before loading.
    #[arg(long)]
    flush: bool,
}

#[derive(Copy, Clone, ValueEnum, PartialEq)]
enum ScrambleOpt {
    Auto,
    On,
    Off,
}

#[derive(Copy, Clone, ValueEnum, PartialEq)]
enum Arrival {
    Uniform,
    Poisson,
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    data: DataArgs,
    #[command(flatten)]
    conn: ConnArgs,
    /// Share of GETs (rest are SETs).
    #[arg(long, default_value_t = 0.9)]
    get_ratio: f64,
    /// Key distribution: uniform | normal:CENTER:STDDEV | zipfian:THETA | hotspot:HOT_KEYS:HOT_OPS[:START]
    /// (CENTER, STDDEV, HOT_KEYS, START are fractions of the key space).
    #[arg(long, default_value = "uniform")]
    key_dist: String,
    /// Scatter hot keys over the key space with a fixed permutation.
    /// auto = on for zipfian, off otherwise.
    #[arg(long, value_enum, default_value_t = ScrambleOpt::Auto)]
    scramble: ScrambleOpt,
    /// Target total ops/s (open loop). 0 = full load (closed loop).
    #[arg(long, default_value_t = 0.0)]
    rate: f64,
    /// Open-loop arrival process.
    #[arg(long, value_enum, default_value_t = Arrival::Uniform)]
    arrival: Arrival,
    /// Closed loop: requests in flight per connection.
    #[arg(long, default_value_t = 1)]
    pipeline: usize,
    /// Open loop: max requests in flight per connection (excess waits, and the wait counts as latency).
    #[arg(long, default_value_t = 1000)]
    max_pending: usize,
    /// Measured seconds.
    #[arg(long, default_value_t = 60.0)]
    time: f64,
    /// Warm-up seconds (run but not counted).
    #[arg(long, default_value_t = 10.0)]
    warmup: f64,
    /// Open loop: busy-wait instead of sleeping when the next send is this close (microseconds).
    /// Higher = sends closer to schedule, more client CPU.
    #[arg(long, default_value_t = 15.0)]
    spin_us: f64,
    /// Check every GET value against the expected content.
    #[arg(long)]
    verify: bool,
}

fn parse_size(s: &str) -> Result<SizeDist, String> {
    let p: Vec<&str> = s.split(':').collect();
    let f = |i: usize| -> Result<f64, String> { p.get(i).ok_or(format!("value-size {s}: missing field"))?.parse::<f64>().map_err(|e| format!("value-size {s}: {e}")) };
    let opt = |i: usize, d: usize| -> Result<usize, String> { match p.get(i) { Some(x) => x.parse().map_err(|e| format!("{e}")), None => Ok(d) } };
    Ok(match p[0] {
        "fixed" => SizeDist::Fixed(f(1)? as usize),
        "uniform" => SizeDist::Uniform { min: f(1)? as usize, max: f(2)? as usize },
        "normal" => {
            let (m, sd) = (f(1)?, f(2)?);
            SizeDist::Normal { mean: m, stddev: sd, min: opt(3, 1)?, max: opt(4, (m + 6.0 * sd) as usize)? }
        }
        "lognormal" => {
            let (m, sg) = (f(1)?, f(2)?);
            SizeDist::LogNormal { median: m, sigma: sg, min: opt(3, 1)?, max: opt(4, ((m * (4.0 * sg).exp()) as usize).min(64 << 20))? }
        }
        _ => return Err(format!("unknown value-size {s}")),
    })
}

fn parse_key_dist(s: &str, n: u64) -> Result<KeyDist, String> {
    let p: Vec<&str> = s.split(':').collect();
    let f = |i: usize, d: Option<f64>| -> Result<f64, String> {
        match p.get(i) {
            Some(x) => x.parse::<f64>().map_err(|e| format!("key-dist {s}: {e}")),
            None => d.ok_or(format!("key-dist {s}: missing field")),
        }
    };
    Ok(match p[0] {
        "uniform" => KeyDist::Uniform,
        "normal" => KeyDist::Normal { center: f(1, Some(0.5))?, stddev: f(2, Some(0.05))? },
        "zipfian" | "zipf" => KeyDist::zipfian(n, f(1, Some(0.99))?),
        "hotspot" => KeyDist::Hotspot { hot_keys: f(1, Some(0.1))?, hot_ops: f(2, Some(0.9))?, hot_start: f(3, Some(0.0))? },
        _ => return Err(format!("unknown key-dist {s}")),
    })
}

fn parse_cpus(s: &str) -> Vec<usize> {
    let mut v = Vec::new();
    for part in s.split(',') {
        if let Some((a, b)) = part.split_once('-') {
            let (a, b): (usize, usize) = (a.trim().parse().unwrap(), b.trim().parse().unwrap());
            v.extend(a..=b);
        } else if !part.trim().is_empty() {
            v.push(part.trim().parse().unwrap());
        }
    }
    v
}

fn pct(h: &Histogram<u64>, q: f64) -> f64 {
    if h.is_empty() { 0.0 } else { h.value_at_quantile(q) as f64 / 1000.0 }
}

fn hist_json(h: &Histogram<u64>) -> Value {
    json!({
        "count": h.len(),
        "mean_us": if h.is_empty() { 0.0 } else { h.mean() / 1000.0 },
        "p50_us": pct(h, 0.50), "p90_us": pct(h, 0.90), "p99_us": pct(h, 0.99),
        "p999_us": pct(h, 0.999), "p9999_us": pct(h, 0.9999),
        "max_us": if h.is_empty() { 0.0 } else { h.max() as f64 / 1000.0 },
    })
}

struct Agg {
    get_rt: Histogram<u64>,
    set_rt: Histogram<u64>,
    get_svc: Histogram<u64>,
    set_svc: Histogram<u64>,
    send_lag: Histogram<u64>,
    misses: u64,
    errors: u64,
    verify_fail: u64,
    backlog: u64,
}

impl Agg {
    fn new() -> Self {
        Agg { get_rt: new_hist(), set_rt: new_hist(), get_svc: new_hist(), set_svc: new_hist(), send_lag: new_hist(), misses: 0, errors: 0, verify_fail: 0, backlog: 0 }
    }
    fn add(&mut self, s: &Snapshot) {
        self.get_rt.add(&s.get_rt).ok();
        self.set_rt.add(&s.set_rt).ok();
        self.get_svc.add(&s.get_svc).ok();
        self.set_svc.add(&s.set_svc).ok();
        self.send_lag.add(&s.send_lag).ok();
        self.misses += s.misses;
        self.errors += s.errors;
        self.verify_fail += s.verify_fail;
        self.backlog += s.backlog;
    }
    fn add_agg(&mut self, s: &Agg) {
        self.get_rt.add(&s.get_rt).ok();
        self.set_rt.add(&s.set_rt).ok();
        self.get_svc.add(&s.get_svc).ok();
        self.set_svc.add(&s.set_svc).ok();
        self.send_lag.add(&s.send_lag).ok();
        self.misses += s.misses;
        self.errors += s.errors;
        self.verify_fail += s.verify_fail;
        self.backlog = s.backlog;
    }
    fn all_rt(&self) -> Histogram<u64> {
        let mut h = self.get_rt.clone();
        h.add(&self.set_rt).ok();
        h
    }
    fn all_svc(&self) -> Histogram<u64> {
        let mut h = self.get_svc.clone();
        h.add(&self.set_svc).ok();
        h
    }
    fn ops(&self) -> u64 {
        self.get_rt.len() + self.set_rt.len()
    }
}

struct Output {
    f: Option<std::fs::File>,
}

impl Output {
    fn new(path: &Option<String>) -> Self {
        Output { f: path.as_ref().map(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).expect("open --out")) }
    }
    fn write(&mut self, v: &Value) {
        if let Some(f) = self.f.as_mut() {
            writeln!(f, "{}", v).ok();
        }
    }
}

fn resolve(c: &ConnArgs) -> Vec<std::net::SocketAddr> {
    c.port
        .split(',')
        .map(|p| format!("{}:{}", c.host, p.trim()).to_socket_addrs().expect("resolve host").next().expect("no address"))
        .collect()
}

fn make_vals(d: &DataArgs) -> Arc<ValueGen> {
    let size = parse_size(&d.value_size).unwrap_or_else(|e| die(&e));
    if !(0.0..=100.0).contains(&d.value_random_pct) {
        die("value-random-pct must be 0..100");
    }
    Arc::new(ValueGen::new(size, d.value_random_pct, d.pool_mb << 20, d.seed))
}

fn die(msg: &str) -> ! {
    eprintln!("kvbench: {msg}");
    std::process::exit(2)
}

fn data_json(d: &DataArgs, vals: &ValueGen) -> Value {
    json!({
        "keys": d.keys, "key_prefix": d.key_prefix, "value_size": vals.size.describe(),
        "value_random_pct": d.value_random_pct, "pool_mb": d.pool_mb, "seed": d.seed,
    })
}

struct Engine {
    sh: Arc<Shared>,
    rx: mpsc::Receiver<Snapshot>,
    handles: Vec<std::thread::JoinHandle<Result<(), String>>>,
    threads: usize,
}

fn start_engine(sh: Shared, c: &ConnArgs) -> Engine {
    let sh = Arc::new(sh);
    let (tx, rx) = mpsc::channel();
    let cpus = c.cpus.as_deref().map(parse_cpus).unwrap_or_default();
    let mut handles = Vec::new();
    for t in 0..c.threads {
        let sh2 = sh.clone();
        let tx2 = tx.clone();
        let cpu = if cpus.is_empty() { None } else { Some(cpus[t % cpus.len()]) };
        handles.push(std::thread::Builder::new().name(format!("kvb-{t}")).spawn(move || {
            let r = worker::run_worker(sh2.clone(), t, cpu, tx2);
            if let Err(e) = &r {
                eprintln!("kvbench: thread {t}: {e}");
                sh2.stop.store(true, Ordering::Relaxed);
            }
            r
        }).unwrap());
    }
    Engine { sh, rx, handles, threads: c.threads }
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Ratio(ra) => {
            let d = ra.data;
            let vals = make_vals(&d);
            let lv: Vec<i32> = ra.zstd_levels.split(',').filter(|x| !x.trim().is_empty()).map(|x| x.trim().parse().expect("zstd level")).collect();
            let r = vals.measure_ratio_ext(d.keys, ra.pages, &lv, ra.dump.as_deref());
            println!("{}", serde_json::to_string_pretty(&json!({"data": data_json(&d, &vals), "compressibility": r})).unwrap());
        }
        Cmd::Load(a) => cmd_load(a),
        Cmd::Run(a) => cmd_run(a),
    }
}

fn base_shared(d: &DataArgs, c: &ConnArgs, vals: Arc<ValueGen>, mode: Mode) -> Shared {
    Shared {
        addrs: resolve(c),
        mode,
        conns: c.conns,
        depth: 1,
        rate_per_thread: None,
        poisson: false,
        max_pending: 1000,
        get_ratio: 0.0,
        keys: d.keys,
        dist: KeyDist::Uniform,
        scramble: None,
        prefix: d.key_prefix.as_bytes().to_vec(),
        vals,
        verify: false,
        stop: AtomicBool::new(false),
        start_ns: AtomicU64::new(0),
        interval_ns: (c.interval * 1e9) as u64,
        load_next: AtomicU64::new(0),
        load_chunk: 4096,
        seed: d.seed,
        spin_ns: 15_000,
    }
}

fn cmd_load(a: LoadArgs) {
    let vals = make_vals(&a.data);
    let mut out = Output::new(&a.conn.out);
    let addrs = resolve(&a.conn);
    let mut info = metrics::InfoConn::new(addrs[0]);
    if a.flush {
        let r = info.cmd(&[b"FLUSHALL"]);
        eprintln!("FLUSHALL: {:?}", r);
    }
    let mut sh = base_shared(&a.data, &a.conn, vals.clone(), Mode::Load);
    sh.depth = a.pipeline;
    let eng = start_engine(sh, &a.conn);
    let t0 = now_ns() + 100_000_000;
    let s0 = metrics::sample(t0, a.conn.server_pid);
    eng.sh.start_ns.store(t0, Ordering::Release);
    let mut finals = 0;
    let mut per_idx: BTreeMap<u64, (usize, Agg)> = BTreeMap::new();
    let mut done = 0u64;
    let mut total = Agg::new();
    while finals < eng.threads {
        let s = match eng.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(s) => s,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        if s.final_ {
            finals += 1;
        }
        total.add(&s);
        let e = per_idx.entry(s.idx).or_insert((0, Agg::new()));
        e.0 += 1;
        e.1.add(&s);
        if e.0 == eng.threads {
            let (_, ag) = per_idx.remove(&s.idx).unwrap();
            done += ag.ops();
            if !a.conn.quiet {
                eprintln!("load t={:>4} {:>10.0} keys/s  {:>5.1}%  p99 {:>8.1} us", s.idx + 1, ag.ops() as f64 / a.conn.interval, 100.0 * done as f64 / a.data.keys as f64, pct(&ag.all_svc(), 0.99));
            }
        }
    }
    for h in eng.handles {
        if let Ok(Err(e)) = h.join() {
            die(&format!("load failed: {e}"));
        }
    }
    let t1 = now_ns();
    let s1 = metrics::sample(t1, a.conn.server_pid);
    let secs = (t1 - t0) as f64 / 1e9;
    let dbsize = info.cmd(&[b"DBSIZE"]);
    let inf = info.info();
    let ratio = vals.measure_ratio(a.data.keys, 512);
    let summary = json!({
        "type": "load_summary", "label": a.conn.label,
        "data": data_json(&a.data, &vals), "compressibility": ratio,
        "keys_loaded": total.ops(), "errors": total.errors, "seconds": secs,
        "keys_per_s": total.ops() as f64 / secs,
        "dbsize": dbsize, "server_info": inf, "system": metrics::delta(&s0, &s1),
    });
    out.write(&summary);
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    if total.errors > 0 {
        std::process::exit(1);
    }
}

fn cmd_run(a: RunArgs) {
    let vals = make_vals(&a.data);
    let mut out = Output::new(&a.conn.out);
    let dist = parse_key_dist(&a.key_dist, a.data.keys).unwrap_or_else(|e| die(&e));
    let scramble_on = match a.scramble {
        ScrambleOpt::On => true,
        ScrambleOpt::Off => false,
        ScrambleOpt::Auto => matches!(dist, KeyDist::Zipfian { .. }),
    };
    let ratio = vals.measure_ratio(a.data.keys, 512);
    let mut sh = base_shared(&a.data, &a.conn, vals.clone(), Mode::Run);
    sh.depth = a.pipeline.max(1);
    sh.rate_per_thread = if a.rate > 0.0 { Some(a.rate / a.conn.threads as f64) } else { None };
    sh.poisson = a.arrival == Arrival::Poisson;
    sh.max_pending = a.max_pending.max(1);
    sh.get_ratio = a.get_ratio;
    sh.dist = dist.clone();
    sh.scramble = if scramble_on { Some(Scramble::new(a.data.keys)) } else { None };
    sh.verify = a.verify;
    sh.spin_ns = (a.spin_us * 1000.0) as u64;
    let config = json!({
        "label": a.conn.label, "data": data_json(&a.data, &vals),
        "get_ratio": a.get_ratio, "key_dist": dist.describe(), "scramble": scramble_on,
        "mode": if a.rate > 0.0 { "open" } else { "closed" }, "target_rate": a.rate,
        "arrival": if sh.poisson { "poisson" } else { "uniform" },
        "threads": a.conn.threads, "conns_per_thread": a.conn.conns, "pipeline": sh.depth,
        "max_pending": sh.max_pending, "spin_us": a.spin_us, "time_s": a.time, "warmup_s": a.warmup,
        "interval_s": a.conn.interval, "verify": a.verify, "cpus": a.conn.cpus,
        "server": format!("{}:{}", a.conn.host, a.conn.port), "server_pid": a.conn.server_pid,
    });
    let addrs = resolve(&a.conn);
    let mut info = metrics::InfoConn::new(addrs[0]);
    let info0 = info.info();
    let eng = start_engine(sh, &a.conn);
    let iv = (a.conn.interval * 1e9) as u64;
    let n_warm = (a.warmup / a.conn.interval).round() as u64;
    let n_meas = (a.time / a.conn.interval).round() as u64;
    let t0 = now_ns() + 200_000_000;
    eng.sh.start_ns.store(t0, Ordering::Release);
    let t_stop = t0 + (n_warm + n_meas) * iv;
    // metric samples at each boundary
    let mut samples: Vec<metrics::Sample> = Vec::new();
    while now_ns() < t0 {
        std::hint::spin_loop();
    }
    samples.push(metrics::sample(now_ns(), a.conn.server_pid));
    let mut per_idx: BTreeMap<u64, (usize, Agg)> = BTreeMap::new();
    let mut total = Agg::new();
    let mut warm = Agg::new();
    let mut finals = 0;
    let mut info_meas_start: Option<Map<String, Value>> = if n_warm == 0 { Some(info0.clone()) } else { None };
    let mut info_meas_end: Option<Map<String, Value>> = None;
    let mut next_sample = t0 + iv;
    let mut stopped = false;
    let mut intervals_done = 0u64;
    let target = a.rate;
    while finals < eng.threads {
        let now = now_ns();
        if now >= next_sample {
            samples.push(metrics::sample(now, a.conn.server_pid));
            let k = samples.len() as u64 - 1; // boundaries passed
            if k == n_warm && info_meas_start.is_none() {
                info_meas_start = Some(info.info());
            }
            if k == n_warm + n_meas {
                info_meas_end = Some(info.info());
            }
            next_sample += iv;
        }
        if !stopped && now >= t_stop {
            eng.sh.stop.store(true, Ordering::Relaxed);
            stopped = true;
        }
        let wait = next_sample.saturating_sub(now_ns()).min(50_000_000);
        let s = match eng.rx.recv_timeout(Duration::from_nanos(wait.max(1000))) {
            Ok(s) => s,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        if s.final_ {
            finals += 1;
            continue; // drain after stop: not counted
        }
        let e = per_idx.entry(s.idx).or_insert((0, Agg::new()));
        e.0 += 1;
        e.1.add(&s);
        if e.0 == eng.threads {
            let idx = s.idx;
            let (_, ag) = per_idx.remove(&idx).unwrap();
            intervals_done = idx + 1;
            if idx >= n_warm + n_meas {
                continue;
            }
            let measured = idx >= n_warm;
            if measured {
                total.add_agg(&ag);
            } else {
                warm.add_agg(&ag);
            }
            let sys = if samples.len() as u64 > idx + 1 { metrics::delta(&samples[idx as usize], &samples[idx as usize + 1]) } else { Map::new() };
            let ops = ag.ops() as f64 / a.conn.interval;
            let all = ag.all_rt();
            let line = json!({
                "type": "interval", "label": a.conn.label, "t": idx + 1, "phase": if measured { "measure" } else { "warmup" },
                "ops_s": ops, "get_s": ag.get_rt.len() as f64 / a.conn.interval, "set_s": ag.set_rt.len() as f64 / a.conn.interval,
                "p50_us": pct(&all, 0.5), "p99_us": pct(&all, 0.99), "p999_us": pct(&all, 0.999), "max_us": pct(&all, 1.0),
                "get_p99_us": pct(&ag.get_rt, 0.99), "set_p99_us": pct(&ag.set_rt, 0.99),
                "svc_p99_us": pct(&ag.all_svc(), 0.99), "send_lag_p99_us": pct(&ag.send_lag, 0.99),
                "misses": ag.misses, "errors": ag.errors, "verify_fail": ag.verify_fail, "backlog": ag.backlog,
                "sys": sys,
            });
            out.write(&line);
            if !a.conn.quiet {
                let srv = line["sys"]["server_cpu_s"].as_f64().map(|x| format!(" srvcpu {:>4.2}", x / a.conn.interval)).unwrap_or_default();
                let sw = line["sys"]["swapin_pages_s"].as_f64().unwrap_or(0.0);
                eprintln!(
                    "{} t={:>4} {:>10.0} ops/s  p50 {:>7.1}  p99 {:>8.1}  p99.9 {:>8.1} us  cli {:>4.2}{} swpin/s {:>7.0}{}{}",
                    if measured { "run " } else { "warm" }, idx + 1, ops, pct(&all, 0.5), pct(&all, 0.99), pct(&all, 0.999),
                    line["sys"]["client_cpu_s"].as_f64().unwrap_or(0.0) / a.conn.interval, srv, sw,
                    if ag.backlog > 0 { format!(" backlog {}", ag.backlog) } else { String::new() },
                    if ag.errors + ag.verify_fail > 0 { format!(" ERR {} VERIFY {}", ag.errors, ag.verify_fail) } else { String::new() },
                );
            }
        }
    }
    let mut failed = None;
    for h in eng.handles {
        if let Ok(Err(e)) = h.join() {
            failed = Some(e);
        }
    }
    let _ = intervals_done;
    // measured window system deltas
    let (i0, i1) = (n_warm as usize, (n_warm + n_meas) as usize);
    let sys = if samples.len() > i1 { metrics::delta(&samples[i0], &samples[i1]) } else { Map::new() };
    let secs = n_meas as f64 * a.conn.interval;
    let ops = total.ops() as f64;
    let ops_s = ops / secs;
    let mut cpu = Map::new();
    if let Some(c) = sys.get("client_cpu_s").and_then(|v| v.as_f64()) {
        cpu.insert("client_cpu_cores".into(), json!(c / secs));
        cpu.insert("client_cpu_us_per_op".into(), json!(c * 1e6 / ops.max(1.0)));
    }
    if let Some(c) = sys.get("client_cpu_s").and_then(|v| v.as_f64()) {
        // the client is the bottleneck if its threads were >90% busy (closed loop only: open loop spins on purpose)
        cpu.insert("client_saturated".into(), json!(a.rate <= 0.0 && c / secs > 0.9 * a.conn.threads as f64));
    }
    if let Some(c) = sys.get("server_cpu_s").and_then(|v| v.as_f64()) {
        cpu.insert("server_cpu_cores".into(), json!(c / secs));
        cpu.insert("server_cpu_us_per_op".into(), json!(c * 1e6 / ops.max(1.0)));
    }
    if let Some(f) = sys.get("server_majflt_total").and_then(|v| v.as_f64()) {
        // page reads from swap per operation (zram/zswap: decompressions)
        cpu.insert("server_majflt_per_op".into(), json!(f / ops.max(1.0)));
    }
    if let (Some(s0), Some(s1)) = (&info_meas_start, &info_meas_end) {
        let g = |m: &Map<String, Value>, k: &str| m.get(k).and_then(|v| v.as_f64());
        if let (Some(u0), Some(s0_), Some(u1), Some(s1_)) = (g(s0, "used_cpu_user"), g(s0, "used_cpu_sys"), g(s1, "used_cpu_user"), g(s1, "used_cpu_sys")) {
            let c = (u1 + s1_) - (u0 + s0_);
            cpu.insert("server_info_cpu_cores".into(), json!(c / secs));
            cpu.insert("server_info_cpu_us_per_op".into(), json!(c * 1e6 / ops.max(1.0)));
        }
    }
    let rate_ok = if target > 0.0 { Value::Bool(ops_s >= 0.99 * target && total.backlog < (target * 0.01) as u64 + 100) } else { Value::Null };
    let summary = json!({
        "type": "summary", "config": config, "compressibility": ratio,
        "measured_s": secs, "ops": total.ops(), "ops_s": ops_s,
        "get_s": total.get_rt.len() as f64 / secs, "set_s": total.set_rt.len() as f64 / secs,
        "target_rate_met": rate_ok,
        "latency": {
            "all": hist_json(&total.all_rt()), "get": hist_json(&total.get_rt), "set": hist_json(&total.set_rt),
            "service_all": hist_json(&total.all_svc()),
            "send_lag": hist_json(&total.send_lag),
            "note": "latency = from scheduled send time (open loop) / actual send (closed loop); service = from actual send",
        },
        "misses": total.misses, "errors": total.errors, "verify_fail": total.verify_fail,
        "final_backlog": total.backlog,
        "cpu": cpu, "system": sys,
        "server_info_start": info_meas_start, "server_info_end": info_meas_end,
        "warmup": { "ops_s": warm.ops() as f64 / (n_warm as f64 * a.conn.interval).max(1e-9), "p99_us": pct(&warm.all_rt(), 0.99) },
        "error": failed,
    });
    out.write(&summary);
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    let all = total.all_rt();
    eprintln!(
        "SUMMARY {:.0} ops/s (get {:.0} set {:.0})  p50 {:.1} p99 {:.1} p99.9 {:.1} us  client {:.2} us/op  server {} us/op{}",
        ops_s, total.get_rt.len() as f64 / secs, total.set_rt.len() as f64 / secs, pct(&all, 0.5), pct(&all, 0.99), pct(&all, 0.999),
        cpu.get("client_cpu_us_per_op").and_then(|v| v.as_f64()).unwrap_or(0.0),
        cpu.get("server_cpu_us_per_op").or(cpu.get("server_info_cpu_us_per_op")).and_then(|v| v.as_f64()).map(|x| format!("{x:.2}")).unwrap_or("?".into()),
        if target > 0.0 { format!("  target {} met={}", target, rate_ok) } else { String::new() },
    );
    if cpu.get("client_saturated").and_then(|v| v.as_bool()) == Some(true) {
        eprintln!("WARNING: client threads were >90% busy: throughput may be limited by the client, add --threads/--cpus");
    }
    if failed.is_some() || total.errors > 0 || total.verify_fail > 0 {
        std::process::exit(1);
    }
}

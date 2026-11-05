//! Compile parsed APO commands into a [`Program`] (a list of DSP stages,
//! shared between instances), and run it with an [`Engine`] (the per-deck
//! state, the [`AudioFilter`] the player calls every 20 ms).

use std::sync::Arc;

use wavyzon_plugin::{AudioFilter, CHANNELS, FRAME_SAMPLES};

use crate::conv::{BLOCK, Convolver, Kernel, Scratch};
use crate::design::{self, Coefs};
use crate::ir::Impulse;
use crate::parse::{self, ALL, Command, Delay, Filter, Mask, ReverbSpec, Term};
use crate::reverb::Freeverb;

/// Most stages in one program (after includes).
pub const MAX_STAGES: usize = 512;
/// Deepest `Include:` nesting.
pub const MAX_INCLUDE_DEPTH: usize = 8;
/// Longest `Delay:`.
pub const MAX_DELAY_MS: f64 = 1000.0;
/// Most FFT partitions one channel may run per frame, all convolutions and
/// GraphicEQs together (about 20 s of impulse response): bounds the CPU a
/// single player can take on the send path.
pub const MAX_PARTITIONS_PER_CHANNEL: usize = 1000;

/// Where `Include:` and `Convolution:` files come from: the preset store,
/// already in memory, so compiling never touches the disk.
pub trait Resolver {
    /// Text of a configuration file, by normalised path under the presets
    /// directory.
    fn text(&self, path: &str) -> Option<Arc<str>>;
    /// An impulse response by normalised path: `None` if there is no such
    /// file, `Some(Err)` if it exists but could not be loaded.
    fn impulse(&self, path: &str) -> Option<Result<Arc<Impulse>, String>>;
}

/// Join `name` (as written in a file, `\` or `/`) to the directory of
/// `origin` and normalise it. `None` when it is absolute or leaves the
/// presets directory.
pub fn join(origin: &str, name: &str) -> Option<String> {
    let name = name.replace('\\', "/");
    if name.starts_with('/') || name.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    let mut parts: Vec<&str> = origin.split('/').filter(|p| !p.is_empty()).collect();
    parts.pop(); // the file name of origin
    for p in name.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// One DSP step.
#[derive(Debug)]
pub enum Stage {
    Gain {
        mask: Mask,
        gain: f32,
    },
    Biquad {
        mask: Mask,
        c: Coefs,
    },
    Iir {
        mask: Mask,
        b: Vec<f64>,
        a: Vec<f64>,
    },
    /// Convolution (an impulse response or a GraphicEQ's FIR) per channel.
    Fir {
        kernels: [Option<Arc<Kernel>>; 2],
    },
    Delay {
        mask: Mask,
        samples: usize,
    },
    /// `Copy:` rows `[from L, from R, constant]` for the targets it sets.
    Copy {
        rows: [Option<[f32; 3]>; 2],
    },
    Reverb(ReverbSpec),
}

impl Stage {
    pub fn name(&self) -> &'static str {
        match self {
            Stage::Gain { .. } => "preamp",
            Stage::Biquad { .. } => "filter",
            Stage::Iir { .. } => "iir",
            Stage::Fir { .. } => "convolution",
            Stage::Delay { .. } => "delay",
            Stage::Copy { .. } => "copy",
            Stage::Reverb(_) => "reverb",
        }
    }
}

/// A compiled configuration.
#[derive(Debug, Default)]
pub struct Program {
    pub stages: Vec<Stage>,
    pub warnings: Vec<String>,
}

impl Program {
    /// Stage names in order, for the plugin's routes.
    pub fn summary(&self) -> Vec<&'static str> {
        self.stages.iter().map(Stage::name).collect()
    }
}

struct Compiler<'a> {
    res: &'a dyn Resolver,
    out: Program,
    mask: Mask,
    stack: Vec<String>,
    partitions: [usize; 2],
}

/// Compile `text`. `origin` is its path under the presets directory (`""`
/// for inline text: includes resolve from the directory's root).
pub fn compile(text: &str, origin: &str, res: &dyn Resolver) -> Result<Program, String> {
    if text.len() > parse::MAX_TEXT_BYTES {
        return Err(format!(
            "configuration is larger than {} KiB",
            parse::MAX_TEXT_BYTES / 1024
        ));
    }
    let mut c = Compiler {
        res,
        out: Program::default(),
        mask: ALL,
        stack: Vec::new(),
        partitions: [0, 0],
    };
    c.file(text, origin)?;
    Ok(c.out)
}

impl Compiler<'_> {
    fn push(&mut self, stage: Stage) -> Result<(), String> {
        if self.out.stages.len() == MAX_STAGES {
            return Err(format!("more than {MAX_STAGES} filters"));
        }
        self.out.stages.push(stage);
        Ok(())
    }

    fn fir(&mut self, kernels: [Option<Arc<Kernel>>; 2]) -> Result<(), String> {
        for (c, k) in kernels.iter().enumerate() {
            if let Some(k) = k {
                self.partitions[c] += k.partitions();
                if self.partitions[c] > MAX_PARTITIONS_PER_CHANNEL {
                    return Err("too much convolution: impulse responses and GraphicEQs add up to more than about 20 s per channel".into());
                }
            }
        }
        if kernels.iter().any(Option::is_some) {
            self.push(Stage::Fir { kernels })?;
        }
        Ok(())
    }

    fn file(&mut self, text: &str, origin: &str) -> Result<(), String> {
        let parsed = parse::parse(text);
        let prefix = if self.stack.is_empty() {
            String::new()
        } else {
            format!("{origin}: ")
        };
        for w in parsed.warnings {
            if self.out.warnings.len() < parse::MAX_WARNINGS {
                self.out.warnings.push(format!("{prefix}{w}"));
            }
        }
        for (line, cmd) in parsed.commands {
            let at = |e: String| format!("{prefix}line {line}: {e}");
            match cmd {
                Command::Preamp(db) => {
                    if db != 0.0 {
                        let gain = 10f64.powf(db / 20.0) as f32;
                        self.push(Stage::Gain { mask: self.mask, gain })?;
                    }
                }
                Command::Filter(Filter::Biquad(spec)) => {
                    let c = design::biquad(&spec);
                    self.push(Stage::Biquad { mask: self.mask, c })?;
                }
                Command::Filter(Filter::Iir { b, a }) => self.push(Stage::Iir { mask: self.mask, b, a })?,
                Command::GraphicEq(points) => {
                    if let Some(taps) = design::graphic_eq_taps(&points) {
                        let k = Arc::new(Kernel::new(&taps));
                        let kernels = [0, 1].map(|c| self.mask[c].then(|| k.clone()));
                        self.fir(kernels).map_err(at)?;
                    }
                }
                Command::Channel(m) => self.mask = m,
                Command::Copy(assigns) => {
                    let mut rows = [None, None];
                    for a in assigns {
                        let mut row = [0.0f32; 3];
                        for t in a.terms {
                            match t {
                                Term::Channel { factor, source } => row[source] += factor as f32,
                                Term::Constant(v) => row[2] += v as f32,
                            }
                        }
                        rows[a.target] = Some(row);
                    }
                    self.push(Stage::Copy { rows })?;
                }
                Command::Delay(d) => {
                    let ms = match d {
                        Delay::Ms(ms) => ms,
                        Delay::Samples(n) => n / 48.0,
                    };
                    if ms > MAX_DELAY_MS {
                        return Err(at(format!("delay longer than {MAX_DELAY_MS} ms")));
                    }
                    let samples = (ms * 48.0).round() as usize;
                    if samples > 0 {
                        self.push(Stage::Delay {
                            mask: self.mask,
                            samples,
                        })?;
                    }
                }
                Command::Include(name) => {
                    let path = join(origin, &name).ok_or_else(|| {
                        at(format!(
                            "Include {name:?} must be a relative path inside the presets directory"
                        ))
                    })?;
                    let text = self
                        .res
                        .text(&path)
                        .ok_or_else(|| at(format!("Include {name:?}: no such file in the presets directory")))?;
                    if self.stack.contains(&path) || path == origin {
                        return Err(at(format!("Include {name:?} includes itself")));
                    }
                    if self.stack.len() == MAX_INCLUDE_DEPTH {
                        return Err(at(format!("Include nested deeper than {MAX_INCLUDE_DEPTH}")));
                    }
                    self.stack.push(origin.to_string());
                    let r = self.file(&text, &path);
                    self.stack.pop();
                    r?;
                }
                Command::Convolution(name) => {
                    let path = join(origin, &name).ok_or_else(|| {
                        at(format!(
                            "Convolution {name:?} must be a relative path inside the presets directory"
                        ))
                    })?;
                    let ir = match self.res.impulse(&path) {
                        Some(Ok(ir)) => ir,
                        Some(Err(e)) => return Err(at(format!("Convolution {name:?}: {e}"))),
                        None => {
                            return Err(at(format!(
                                "Convolution {name:?}: no such file in the presets directory"
                            )));
                        }
                    };
                    // IR channels go to the selected channels round-robin.
                    let mut kernels = [None, None];
                    let mut k = 0;
                    for (c, slot) in kernels.iter_mut().enumerate() {
                        if self.mask[c] {
                            *slot = Some(ir.channels[k % ir.channels.len()].clone());
                            k += 1;
                        }
                    }
                    self.fir(kernels).map_err(at)?;
                }
                Command::Reverb(spec) => {
                    if spec.mix > 0.0 {
                        self.push(Stage::Reverb(spec))?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Biquad state, transposed direct form II.
#[derive(Debug, Clone, Copy, Default)]
struct Tdf2 {
    z1: f64,
    z2: f64,
}

/// Values this small only cost CPU (denormals) once a filter has rung out.
const DENORMAL: f64 = 1e-25;

fn run_biquad(c: &Coefs, s: &mut Tdf2, buf: &mut [f32]) {
    for x in buf {
        let xi = *x as f64;
        let y = c.b0 * xi + s.z1;
        s.z1 = c.b1 * xi - c.a1 * y + s.z2;
        s.z2 = c.b2 * xi - c.a2 * y;
        *x = y as f32;
    }
    if s.z1.abs() < DENORMAL {
        s.z1 = 0.0;
    }
    if s.z2.abs() < DENORMAL {
        s.z2 = 0.0;
    }
}

fn run_iir(b: &[f64], a: &[f64], z: &mut [f64], buf: &mut [f32]) {
    let n = z.len();
    for x in buf {
        let xi = *x as f64;
        let y = b[0] * xi + z[0];
        for i in 0..n {
            let next = if i + 1 < n { z[i + 1] } else { 0.0 };
            z[i] = b[i + 1] * xi - a[i + 1] * y + next;
        }
        *x = y as f32;
    }
    for v in z.iter_mut() {
        if v.abs() < DENORMAL {
            *v = 0.0;
        }
    }
}

struct Ring {
    buf: Vec<f32>,
    idx: usize,
}

impl Ring {
    fn run(&mut self, x: &mut [f32]) {
        for s in x {
            std::mem::swap(&mut self.buf[self.idx], s);
            self.idx += 1;
            if self.idx == self.buf.len() {
                self.idx = 0;
            }
        }
    }
}

enum State {
    None,
    Biquad([Tdf2; 2]),
    Iir([Vec<f64>; 2]),
    Fir([Option<Convolver>; 2]),
    Delay([Option<Ring>; 2]),
    Reverb(Box<Freeverb>),
}

/// A running program: one per deck. Everything is allocated in
/// [`new`](Self::new); [`process`](AudioFilter::process) does not allocate.
pub struct Engine {
    program: Arc<Program>,
    states: Vec<State>,
    ch: [Vec<f32>; CHANNELS],
    scratch: Scratch,
}

impl Engine {
    pub fn new(program: Arc<Program>) -> Self {
        let states = program
            .stages
            .iter()
            .map(|s| match s {
                Stage::Gain { .. } | Stage::Copy { .. } => State::None,
                Stage::Biquad { .. } => State::Biquad([Tdf2::default(); 2]),
                Stage::Iir { a, .. } => State::Iir([vec![0.0; a.len() - 1], vec![0.0; a.len() - 1]]),
                Stage::Fir { kernels } => State::Fir(kernels.clone().map(|k| k.map(Convolver::new))),
                Stage::Delay { mask, samples } => State::Delay([0, 1].map(|c| {
                    mask[c].then(|| Ring {
                        buf: vec![0.0; *samples],
                        idx: 0,
                    })
                })),
                Stage::Reverb(spec) => State::Reverb(Box::new(Freeverb::new(*spec))),
            })
            .collect();
        Self {
            program,
            states,
            ch: [vec![0.0; BLOCK], vec![0.0; BLOCK]],
            scratch: Scratch::default(),
        }
    }

    pub fn program(&self) -> &Arc<Program> {
        &self.program
    }
}

impl AudioFilter for Engine {
    fn process(&mut self, frame: &mut [f32]) {
        // The plugin API always hands 20 ms stereo frames; the convolver
        // needs exactly that.
        if frame.len() != FRAME_SAMPLES {
            return;
        }
        let [l, r] = &mut self.ch;
        for (i, pair) in frame.chunks_exact(CHANNELS).enumerate() {
            l[i] = pair[0];
            r[i] = pair[1];
        }
        for (stage, state) in self.program.stages.iter().zip(self.states.iter_mut()) {
            match (stage, state) {
                (Stage::Gain { mask, gain }, _) => {
                    for (c, buf) in [&mut *l, &mut *r].into_iter().enumerate() {
                        if mask[c] {
                            buf.iter_mut().for_each(|s| *s *= gain);
                        }
                    }
                }
                (Stage::Biquad { mask, c }, State::Biquad(st)) => {
                    for (ch, buf) in [&mut *l, &mut *r].into_iter().enumerate() {
                        if mask[ch] {
                            run_biquad(c, &mut st[ch], buf);
                        }
                    }
                }
                (Stage::Iir { mask, b, a }, State::Iir(z)) => {
                    for (ch, buf) in [&mut *l, &mut *r].into_iter().enumerate() {
                        if mask[ch] {
                            run_iir(b, a, &mut z[ch], buf);
                        }
                    }
                }
                (Stage::Fir { .. }, State::Fir(convs)) => {
                    for (conv, buf) in convs.iter_mut().zip([&mut *l, &mut *r]) {
                        if let Some(conv) = conv {
                            conv.process(buf, &mut self.scratch);
                        }
                    }
                }
                (Stage::Delay { .. }, State::Delay(rings)) => {
                    for (ring, buf) in rings.iter_mut().zip([&mut *l, &mut *r]) {
                        if let Some(ring) = ring {
                            ring.run(buf);
                        }
                    }
                }
                (Stage::Copy { rows }, _) => {
                    for i in 0..BLOCK {
                        let (a, b) = (l[i], r[i]);
                        if let Some(row) = rows[0] {
                            l[i] = row[0] * a + row[1] * b + row[2];
                        }
                        if let Some(row) = rows[1] {
                            r[i] = row[0] * a + row[1] * b + row[2];
                        }
                    }
                }
                (Stage::Reverb(_), State::Reverb(rv)) => rv.process(l, r),
                _ => {}
            }
        }
        for (i, pair) in frame.chunks_exact_mut(CHANNELS).enumerate() {
            // An unstable custom IIR must not poison the output.
            pair[0] = if l[i].is_finite() { l[i] } else { 0.0 };
            pair[1] = if r[i].is_finite() { r[i] } else { 0.0 };
        }
    }

    fn reset(&mut self) {
        for s in &mut self.states {
            match s {
                State::None => {}
                State::Biquad(st) => *st = [Tdf2::default(); 2],
                State::Iir(z) => z.iter_mut().for_each(|z| z.fill(0.0)),
                State::Fir(convs) => convs.iter_mut().flatten().for_each(Convolver::reset),
                State::Delay(rings) => rings.iter_mut().flatten().for_each(|r| {
                    r.buf.fill(0.0);
                    r.idx = 0;
                }),
                State::Reverb(rv) => rv.reset(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct Files {
        texts: HashMap<String, Arc<str>>,
        irs: HashMap<String, Arc<Impulse>>,
    }

    impl Resolver for Files {
        fn text(&self, path: &str) -> Option<Arc<str>> {
            self.texts.get(path).cloned()
        }
        fn impulse(&self, path: &str) -> Option<Result<Arc<Impulse>, String>> {
            self.irs.get(path).cloned().map(Ok)
        }
    }

    fn impulse(channels: &[&[f32]]) -> Arc<Impulse> {
        Arc::new(Impulse {
            channels: channels.iter().map(|c| Arc::new(Kernel::new(c))).collect(),
            source_rate: 48_000,
            frames: channels[0].len(),
            dropped_channels: 0,
            truncated: false,
        })
    }

    fn engine(text: &str, files: &Files) -> Engine {
        Engine::new(Arc::new(compile(text, "", files).unwrap()))
    }

    /// Run a frame of constant (l, r) through and return the last sample.
    fn steady(e: &mut Engine, l: f32, r: f32, frames: usize) -> (f32, f32) {
        let mut out = (0.0, 0.0);
        for _ in 0..frames {
            let mut f: Vec<f32> = (0..FRAME_SAMPLES).map(|i| if i % 2 == 0 { l } else { r }).collect();
            e.process(&mut f);
            out = (f[FRAME_SAMPLES - 2], f[FRAME_SAMPLES - 1]);
        }
        out
    }

    #[test]
    fn join_stays_inside_the_directory() {
        assert_eq!(join("", "a.txt").as_deref(), Some("a.txt"));
        assert_eq!(
            join("hp/hd600.txt", "common\\pre.txt").as_deref(),
            Some("hp/common/pre.txt")
        );
        assert_eq!(join("hp/hd600.txt", "../ir/hall.wav").as_deref(), Some("ir/hall.wav"));
        assert_eq!(join("hp/hd600.txt", "../../x"), None);
        assert_eq!(join("", "/etc/passwd"), None);
        assert_eq!(join("", "C:\\Program Files\\EqualizerAPO\\config\\x.txt"), None);
    }

    #[test]
    fn preamp_channel_and_copy() {
        let files = Files::default();
        let mut e = engine("Channel: L\nPreamp: -6.0206 dB\nChannel: all", &files);
        let (l, r) = steady(&mut e, 0.8, 0.8, 1);
        assert!((l - 0.4).abs() < 1e-4 && (r - 0.8).abs() < 1e-6);
        // Assignments on one line see the input, so this swaps.
        let mut e = engine("Copy: L=R R=L", &files);
        assert_eq!(steady(&mut e, 0.1, 0.2, 1), (0.2, 0.1));
        let mut e = engine("Copy: L=0.5*L+0.5*R R=0.25", &files);
        let (l, r) = steady(&mut e, 0.2, 0.4, 1);
        assert!((l - 0.3).abs() < 1e-6 && (r - 0.25).abs() < 1e-6);
    }

    #[test]
    fn filters_shape_the_signal() {
        let files = Files::default();
        // A high-pass removes DC.
        let mut e = engine("Filter: ON HP Fc 100 Hz", &files);
        let (l, _) = steady(&mut e, 0.5, 0.5, 50);
        assert!(l.abs() < 1e-3, "{l}");
        // A low shelf boosts DC by its gain.
        let mut e = engine("Filter: ON LS Fc 300 Hz Gain 6.0206 dB", &files);
        let (l, _) = steady(&mut e, 0.25, 0.25, 50);
        assert!((l - 0.5).abs() < 1e-3, "{l}");
        // A custom IIR (one-pole average) passes DC.
        let mut e = engine("Filter: ON IIR Order 1 Coefficients 0.5 0 1 -0.5", &files);
        let (l, _) = steady(&mut e, 0.3, 0.3, 5);
        assert!((l - 0.3).abs() < 1e-4, "{l}");
        // GraphicEQ: -6 dB everywhere halves DC (after the FIR settles).
        let mut e = engine("GraphicEQ: 20 -6.0206; 20000 -6.0206", &files);
        let (l, _) = steady(&mut e, 0.5, 0.5, 20);
        assert!((l - 0.25).abs() < 2e-3, "{l}");
    }

    #[test]
    fn delay_shifts_by_whole_samples() {
        let files = Files::default();
        let mut e = engine("Channel: R\nDelay: 10 samples", &files);
        let mut f = vec![0.0f32; FRAME_SAMPLES];
        f[0] = 1.0;
        f[1] = 1.0;
        e.process(&mut f);
        assert_eq!(f[0], 1.0, "left untouched");
        assert_eq!(f[1], 0.0);
        assert_eq!(f[2 * 10 + 1], 1.0, "right delayed by 10 samples");
    }

    #[test]
    fn convolution_round_robins_ir_channels_and_includes_resolve() {
        let mut files = Files::default();
        // Left IR: identity. Right IR: -1 (inverts).
        files.irs.insert("ir/hall.wav".into(), impulse(&[&[1.0], &[-1.0]]));
        files
            .texts
            .insert("ir/rev.txt".into(), Arc::from("Convolution: hall.wav"));
        let mut e = engine("Include: ir/rev.txt", &files);
        let (l, r) = steady(&mut e, 0.5, 0.5, 1);
        assert!((l - 0.5).abs() < 1e-4 && (r + 0.5).abs() < 1e-4, "{l} {r}");
        // Only R selected: it takes the IR's first channel.
        let mut e = engine("Channel: R\nConvolution: ir/hall.wav", &files);
        let (l, r) = steady(&mut e, 0.5, 0.5, 1);
        assert!((l - 0.5).abs() < 1e-6 && (r - 0.5).abs() < 1e-4);
    }

    #[test]
    fn missing_files_cycles_and_limits_are_errors() {
        let mut files = Files::default();
        files.texts.insert("a.txt".into(), Arc::from("Include: b.txt"));
        files.texts.insert("b.txt".into(), Arc::from("Include: a.txt"));
        assert!(
            compile("Include: nope.txt", "", &files)
                .unwrap_err()
                .contains("no such file")
        );
        assert!(
            compile("Convolution: nope.wav", "", &files)
                .unwrap_err()
                .contains("no such file")
        );
        assert!(
            compile("Include: ../x.txt", "", &files)
                .unwrap_err()
                .contains("relative path")
        );
        assert!(
            compile("Include: b.txt", "a.txt", &files)
                .unwrap_err()
                .contains("includes itself")
        );
        assert!(compile("Delay: 2000 ms", "", &files).is_err());
        let many = "Filter: ON PK Fc 100 Hz Gain 1 dB Q 1\n".repeat(MAX_STAGES + 1);
        assert!(compile(&many, "", &files).unwrap_err().contains("filters"));
        assert!(compile(&"#".repeat(parse::MAX_TEXT_BYTES + 1), "", &files).is_err());
    }

    #[test]
    fn warnings_carry_the_included_file_name() {
        let mut files = Files::default();
        files.texts.insert("sub/x.txt".into(), Arc::from("Foo: bar"));
        let p = compile("Bar: baz\nInclude: sub/x.txt", "", &files).unwrap();
        assert_eq!(p.warnings.len(), 2);
        assert!(p.warnings[1].starts_with("sub/x.txt: line 1"), "{:?}", p.warnings);
    }

    #[test]
    fn reverb_rings_and_reset_silences() {
        let files = Files::default();
        let mut e = engine("Reverb: Mix 50 % Room 80 %", &files);
        assert_eq!(e.program().summary(), vec!["reverb"]);
        let mut f = vec![0.0f32; FRAME_SAMPLES];
        f[0] = 1.0;
        f[1] = 1.0;
        e.process(&mut f);
        let mut g = vec![0.0f32; FRAME_SAMPLES];
        e.process(&mut g);
        assert!(g.iter().any(|v| v.abs() > 1e-4));
        e.reset();
        let mut h = vec![0.0f32; FRAME_SAMPLES];
        e.process(&mut h);
        assert!(h.iter().all(|v| *v == 0.0));
    }

    /// `cargo test --release -p wavyzon-plugin-apo -- --ignored --nocapture cost`
    #[test]
    #[ignore]
    fn cost_of_a_heavy_program_per_frame() {
        let mut files = Files::default();
        let ir: Vec<f32> = (0..3 * 48_000)
            .map(|i| (-(i as f32) / 20_000.0).exp() * ((i * 7919 % 1000) as f32 / 1000.0 - 0.5))
            .collect();
        files.irs.insert("hall.wav".into(), impulse(&[&ir, &ir]));
        let mut text = String::from("Preamp: -6 dB\n");
        for i in 0..10 {
            text.push_str(&format!(
                "Filter {i}: ON PK Fc {} Hz Gain 2 dB Q 1.2\n",
                30 * (i + 1) * (i + 1)
            ));
        }
        text.push_str("GraphicEQ: 20 3; 100 0; 1000 -2; 10000 2; 20000 0\nConvolution: hall.wav\nReverb: Mix 20 %\n");
        let mut e = engine(&text, &files);
        let mut f: Vec<f32> = (0..FRAME_SAMPLES).map(|i| ((i as f32) * 0.01).sin() * 0.3).collect();
        let n = 500;
        let t = std::time::Instant::now();
        for _ in 0..n {
            e.process(&mut f);
        }
        let per = t.elapsed() / n;
        println!(
            "{:?} per 20 ms frame ({:.1} % of real time)",
            per,
            per.as_secs_f64() / 0.02 * 100.0
        );
    }

    #[test]
    fn unstable_iir_cannot_produce_nan() {
        let files = Files::default();
        let mut e = engine("Filter: ON IIR Order 1 Coefficients 1 0 1 -1.5", &files);
        for _ in 0..200 {
            let mut f = vec![1.0f32; FRAME_SAMPLES];
            e.process(&mut f);
            assert!(f.iter().all(|v| v.is_finite()));
        }
    }
}

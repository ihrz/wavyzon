//! Parser for Equalizer APO configuration text.
//!
//! Follows the "Configuration reference" of Equalizer APO: lines are
//! `Command: parameters`, `#` starts a comment, lines that don't look like a
//! command and unknown commands are ignored. A command we understand but
//! whose parameters are malformed is skipped with a warning, as APO skips it
//! (APO logs it instead). Nothing here touches the disk or builds DSP.

/// Longest configuration text accepted (inline or from a file).
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Most warnings kept per configuration.
pub const MAX_WARNINGS: usize = 100;
/// Most points in one `GraphicEQ:` line.
pub const MAX_GRAPHIC_EQ_POINTS: usize = 4096;
/// Highest order of an `IIR` filter.
pub const MAX_IIR_ORDER: usize = 16;

/// Left and right, the only channels a Wavyzon frame has.
pub type Mask = [bool; 2];
pub const ALL: Mask = [true, true];

/// One understood command, in file order.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `Preamp: -6 dB`
    Preamp(f64),
    Filter(Filter),
    /// `GraphicEQ: 25 6; 40 4.5; ...` as (Hz, dB) points.
    GraphicEq(Vec<(f64, f64)>),
    /// `Channel: L` — the channels following commands apply to.
    Channel(Mask),
    /// `Copy: L=R R=L` — assignments evaluated together.
    Copy(Vec<Assign>),
    Delay(Delay),
    /// `Include: other.txt`
    Include(String),
    /// `Convolution: hall.wav`
    Convolution(String),
    /// `Reverb: Mix 30 % Room 70 % Damping 50 %` (Wavyzon extension; APO
    /// ignores unknown commands, so the file stays APO-compatible).
    Reverb(ReverbSpec),
}

/// A `Filter:` line.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Biquad(BiquadSpec),
    /// `ON IIR Order n Coefficients b0..bn a0..an`, normalised by `a0`.
    Iir {
        b: Vec<f64>,
        a: Vec<f64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Peaking,
    LowPass,
    HighPass,
    BandPass,
    Notch,
    AllPass,
    /// `LS`/`LSC`: second-order shelf, `fc` at the middle of the slope.
    LowShelf,
    HighShelf,
    /// `LS 6dB` / `HS 6dB`: first-order shelf, `fc` is the corner.
    LowShelf6,
    HighShelf6,
    /// `LS 12dB` / `HS 12dB`: second-order shelf, `fc` is the corner.
    LowShelf12,
    HighShelf12,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BiquadSpec {
    pub kind: Kind,
    pub fc: f64,
    pub gain_db: f64,
    pub q: Option<f64>,
    pub bw_oct: Option<f64>,
    /// `LSC x dB` / `HSC x dB`: slope in dB per octave.
    pub slope_db: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Delay {
    Ms(f64),
    Samples(f64),
}

/// `target = term + term + ...`
#[derive(Debug, Clone, PartialEq)]
pub struct Assign {
    /// 0 = left, 1 = right.
    pub target: usize,
    pub terms: Vec<Term>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Term {
    Channel { factor: f64, source: usize },
    Constant(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReverbSpec {
    pub mix: f64,
    pub room: f64,
    pub damping: f64,
}

impl Default for ReverbSpec {
    fn default() -> Self {
        Self {
            mix: 0.3,
            room: 0.5,
            damping: 0.5,
        }
    }
}

/// The result of parsing: commands with their 1-based line number, and
/// warnings about everything skipped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub commands: Vec<(usize, Command)>,
    pub warnings: Vec<String>,
}

impl Parsed {
    fn warn(&mut self, line: usize, msg: impl Into<String>) {
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(format!("line {line}: {}", msg.into()));
        }
    }
}

/// Parse a configuration. Never fails: problems become warnings.
pub fn parse(text: &str) -> Parsed {
    let mut out = Parsed::default();
    // Depth of If/ElseIf/Else blocks being skipped (conditions aren't
    // evaluated, so their content can't be applied).
    let mut skip_depth = 0usize;
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = match raw.find('#') {
            Some(p) => &raw[..p],
            None => raw,
        }
        .trim();
        let Some((name, params)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        // "Filter 1:" — the number after the command name is not interpreted.
        let Some(cmd) = name.split_whitespace().next() else {
            continue;
        };
        if name.split_whitespace().count() > 2 {
            continue;
        }
        let cmd = cmd.to_ascii_lowercase();
        match cmd.as_str() {
            "if" => {
                if skip_depth == 0 {
                    out.warn(line_no, "If blocks are not evaluated; the block is skipped");
                }
                skip_depth += 1;
                continue;
            }
            "endif" => {
                skip_depth = skip_depth.saturating_sub(1);
                continue;
            }
            "elseif" | "else" => continue,
            _ => {}
        }
        if skip_depth > 0 {
            continue;
        }
        if params.contains('`') {
            out.warn(line_no, "inline expressions are not supported; line skipped");
            continue;
        }
        let params = params.trim();
        let parsed = match cmd.as_str() {
            "preamp" => preamp(params).map(Command::Preamp),
            "filter" => match filter(params) {
                Ok(Some(f)) => Ok(Command::Filter(f)),
                Ok(None) => continue, // OFF
                Err(e) => Err(e),
            },
            "graphiceq" => graphic_eq(params).map(Command::GraphicEq),
            "channel" => channel(params, &mut out, line_no).map(Command::Channel),
            "copy" => copy(params, &mut out, line_no).map(Command::Copy),
            "delay" => delay(params).map(Command::Delay),
            "include" => path_param(params).map(Command::Include),
            "convolution" => path_param(params).map(Command::Convolution),
            "reverb" => reverb(params).map(Command::Reverb),
            "device" | "stage" => {
                out.warn(line_no, format!("{name} is not supported; it is ignored"));
                continue;
            }
            "eval" | "vstplugin" | "loudnesscorrection" => {
                out.warn(line_no, format!("{name} is not supported; line skipped"));
                continue;
            }
            _ => {
                out.warn(line_no, format!("unknown command {name:?} ignored"));
                continue;
            }
        };
        match parsed {
            Ok(c) => out.commands.push((line_no, c)),
            Err(e) => out.warn(line_no, format!("{name}: {e}; line skipped")),
        }
    }
    if skip_depth > 0 {
        let n = text.lines().count();
        out.warn(n, "If block without EndIf");
    }
    out
}

fn num(tok: &str) -> Result<f64, String> {
    let v: f64 = tok.parse().map_err(|_| format!("{tok:?} is not a number"))?;
    if v.is_finite() {
        Ok(v)
    } else {
        Err(format!("{tok:?} is not a finite number"))
    }
}

/// `6dB`, `-3.5db` → the number; `None` when the token has no dB suffix.
fn db_suffixed(tok: &str) -> Option<f64> {
    let lower = tok.to_ascii_lowercase();
    lower
        .strip_suffix("db")
        .and_then(|n| n.parse().ok())
        .filter(|v: &f64| v.is_finite())
}

fn preamp(params: &str) -> Result<f64, String> {
    let mut it = params.split_whitespace();
    let first = it.next().ok_or("missing value")?;
    if let Some(v) = db_suffixed(first) {
        return Ok(v);
    }
    num(first)
}

fn filter(params: &str) -> Result<Option<Filter>, String> {
    let toks: Vec<&str> = params.split_whitespace().collect();
    let state = toks.first().ok_or("empty filter")?;
    match state.to_ascii_uppercase().as_str() {
        "ON" => {}
        "OFF" => return Ok(None),
        _ => return Err(format!("expected ON or OFF, found {state:?}")),
    }
    let ty = toks.get(1).ok_or("missing filter type")?.to_ascii_uppercase();
    let mut i = 2;
    if ty == "IIR" {
        return iir(&toks[2..]).map(Some);
    }
    // Shelf slope right after the type: "LS 6dB", "LSC 10.8 dB".
    let mut slope = None;
    if matches!(ty.as_str(), "LS" | "HS" | "LSC" | "HSC")
        && let Some(t) = toks.get(i)
    {
        if let Some(v) = db_suffixed(t) {
            slope = Some(v);
            i += 1;
        } else if let Ok(v) = num(t)
            && toks.get(i + 1).is_some_and(|u| u.eq_ignore_ascii_case("db"))
        {
            slope = Some(v);
            i += 2;
        }
    }
    let (mut fc, mut gain, mut q, mut bw) = (None, None, None, None);
    while i < toks.len() {
        let key = toks[i].to_ascii_uppercase();
        let value = |j: usize| {
            toks.get(j)
                .ok_or(format!("missing value after {}", toks[i]))
                .and_then(|t| num(t))
        };
        match key.as_str() {
            "FC" => {
                fc = Some(value(i + 1)?);
                i += 2;
                if toks.get(i).is_some_and(|t| t.eq_ignore_ascii_case("hz")) {
                    i += 1;
                }
            }
            "GAIN" => {
                let t = toks.get(i + 1).ok_or("missing value after Gain")?;
                if let Some(v) = db_suffixed(t) {
                    gain = Some(v);
                    i += 2;
                } else {
                    gain = Some(num(t)?);
                    i += 2;
                    if toks.get(i).is_some_and(|t| t.eq_ignore_ascii_case("db")) {
                        i += 1;
                    }
                }
            }
            "Q" => {
                q = Some(value(i + 1)?);
                i += 2;
            }
            "BW" => {
                // "BW Oct 0.167"
                if toks.get(i + 1).is_some_and(|t| t.eq_ignore_ascii_case("oct")) {
                    bw = Some(value(i + 2)?);
                    i += 3;
                } else {
                    bw = Some(value(i + 1)?);
                    i += 2;
                }
            }
            // Modal filters: "T60 target 100 ms" only matters to REW.
            "T60" => {
                i += 1;
                while i < toks.len() && !matches!(toks[i].to_ascii_uppercase().as_str(), "FC" | "GAIN" | "Q" | "BW") {
                    i += 1;
                }
            }
            _ => return Err(format!("unexpected {:?}", toks[i])),
        }
    }
    let kind = match ty.as_str() {
        "PK" | "PEQ" | "MODAL" => Kind::Peaking,
        "LP" | "LPQ" => Kind::LowPass,
        "HP" | "HPQ" => Kind::HighPass,
        "BP" => Kind::BandPass,
        "NO" => Kind::Notch,
        "AP" => Kind::AllPass,
        "LS" if slope == Some(6.0) => Kind::LowShelf6,
        "LS" if slope == Some(12.0) => Kind::LowShelf12,
        "HS" if slope == Some(6.0) => Kind::HighShelf6,
        "HS" if slope == Some(12.0) => Kind::HighShelf12,
        "LS" | "LSC" => Kind::LowShelf,
        "HS" | "HSC" => Kind::HighShelf,
        other => return Err(format!("unknown filter type {other:?}")),
    };
    let fc = fc.ok_or("missing Fc")?;
    if !(fc > 0.0 && fc < 1_000_000.0) {
        return Err(format!("Fc {fc} Hz is out of range"));
    }
    let needs_gain = matches!(
        kind,
        Kind::Peaking
            | Kind::LowShelf
            | Kind::HighShelf
            | Kind::LowShelf6
            | Kind::HighShelf6
            | Kind::LowShelf12
            | Kind::HighShelf12
    );
    if needs_gain && gain.is_none() {
        return Err("missing Gain".into());
    }
    if let Some(g) = gain
        && !(-60.0..=60.0).contains(&g)
    {
        return Err(format!("Gain {g} dB is out of range (-60..60)"));
    }
    if matches!(kind, Kind::Peaking | Kind::AllPass) && q.is_none() && bw.is_none() {
        return Err("missing Q or BW".into());
    }
    if let Some(v) = q
        && !(v > 0.0 && v <= 1000.0)
    {
        return Err(format!("Q {v} is out of range"));
    }
    if let Some(v) = bw
        && !(v > 0.0 && v <= 20.0)
    {
        return Err(format!("BW {v} is out of range"));
    }
    let slope_db = match (kind, slope) {
        (Kind::LowShelf | Kind::HighShelf, Some(s)) => {
            if !(s > 0.0 && s <= 24.0) {
                return Err(format!("shelf slope {s} dB is out of range"));
            }
            Some(s)
        }
        _ => None,
    };
    Ok(Some(Filter::Biquad(BiquadSpec {
        kind,
        fc,
        gain_db: if needs_gain { gain.unwrap_or(0.0) } else { 0.0 },
        q,
        bw_oct: bw,
        slope_db,
    })))
}

fn iir(toks: &[&str]) -> Result<Filter, String> {
    if !toks.first().is_some_and(|t| t.eq_ignore_ascii_case("order")) {
        return Err("expected Order".into());
    }
    let order = num(toks.get(1).ok_or("missing order")?)?;
    if order.fract() != 0.0 || !(1.0..=MAX_IIR_ORDER as f64).contains(&order) {
        return Err(format!("order must be an integer between 1 and {MAX_IIR_ORDER}"));
    }
    let order = order as usize;
    if !toks.get(2).is_some_and(|t| t.eq_ignore_ascii_case("coefficients")) {
        return Err("expected Coefficients".into());
    }
    let coefs = toks[3..].iter().map(|t| num(t)).collect::<Result<Vec<f64>, _>>()?;
    if coefs.len() != 2 * (order + 1) {
        return Err(format!(
            "{} coefficients given, {} expected",
            coefs.len(),
            2 * (order + 1)
        ));
    }
    let (b, a) = coefs.split_at(order + 1);
    let a0 = a[0];
    if a0 == 0.0 {
        return Err("a0 must not be zero".into());
    }
    Ok(Filter::Iir {
        b: b.iter().map(|v| v / a0).collect(),
        a: a.iter().map(|v| v / a0).collect(),
    })
}

fn graphic_eq(params: &str) -> Result<Vec<(f64, f64)>, String> {
    let mut points = Vec::new();
    for item in params.split(';') {
        let mut it = item.split_whitespace();
        let Some(f) = it.next() else { continue };
        let g = it.next().ok_or(format!("point {f:?} has no gain"))?;
        let (f, g) = (num(f)?, num(g)?);
        if f <= 0.0 {
            return Err(format!("frequency {f} must be positive"));
        }
        if !(-60.0..=60.0).contains(&g) {
            return Err(format!("gain {g} dB is out of range (-60..60)"));
        }
        if points.len() == MAX_GRAPHIC_EQ_POINTS {
            return Err(format!("more than {MAX_GRAPHIC_EQ_POINTS} points"));
        }
        points.push((f, g));
    }
    if points.is_empty() {
        return Err("no points".into());
    }
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(points)
}

/// L/R by name or number; `None` for channels a stereo frame doesn't have.
fn channel_index(tok: &str) -> Option<usize> {
    match tok.to_ascii_uppercase().as_str() {
        "L" | "1" => Some(0),
        "R" | "2" => Some(1),
        _ => None,
    }
}

fn channel(params: &str, out: &mut Parsed, line: usize) -> Result<Mask, String> {
    let mut mask = [false; 2];
    let mut any = false;
    for tok in params.split_whitespace() {
        any = true;
        if tok.eq_ignore_ascii_case("all") {
            mask = ALL;
        } else if let Some(c) = channel_index(tok) {
            mask[c] = true;
        } else {
            out.warn(line, format!("channel {tok:?} does not exist in stereo; ignored"));
        }
    }
    if !any {
        return Err("no channel given".into());
    }
    Ok(mask)
}

fn copy(params: &str, out: &mut Parsed, line: usize) -> Result<Vec<Assign>, String> {
    let mut assigns = Vec::new();
    for item in params.split_whitespace() {
        let (target, expr) = item.split_once('=').ok_or(format!("{item:?} is not an assignment"))?;
        let Some(target) = channel_index(target) else {
            out.warn(
                line,
                format!("target channel {target:?} does not exist in stereo; ignored"),
            );
            continue;
        };
        let mut terms = Vec::new();
        for term in expr.split('+').filter(|t| !t.is_empty()) {
            let (factor, source) = match term.split_once('*') {
                Some((f, s)) => {
                    let factor = match db_suffixed(f) {
                        Some(db) => 10f64.powf(db / 20.0),
                        None => num(f)?,
                    };
                    (factor, s)
                }
                None => match term.strip_prefix('-') {
                    Some(s) if channel_index(s).is_some() => (-1.0, s),
                    _ => (1.0, term),
                },
            };
            // A constant must contain a decimal point ("2" is channel 2).
            if term.contains('.') && !term.contains('*') {
                terms.push(Term::Constant(num(term)?));
                continue;
            }
            match channel_index(source) {
                Some(c) => terms.push(Term::Channel { factor, source: c }),
                None => out.warn(
                    line,
                    format!("source channel {source:?} does not exist in stereo; treated as silence"),
                ),
            }
        }
        assigns.push(Assign { target, terms });
    }
    if assigns.is_empty() {
        return Err("no assignment".into());
    }
    Ok(assigns)
}

fn delay(params: &str) -> Result<Delay, String> {
    let mut it = params.split_whitespace();
    let v = num(it.next().ok_or("missing value")?)?;
    if v < 0.0 {
        return Err("delay must not be negative".into());
    }
    match it.next().map(|u| u.to_ascii_lowercase()).as_deref() {
        Some("ms") => Ok(Delay::Ms(v)),
        Some("samples") | Some("sample") => Ok(Delay::Samples(v)),
        _ => Err("unit must be ms or samples".into()),
    }
}

fn path_param(params: &str) -> Result<String, String> {
    let p = params.trim().trim_matches('"');
    if p.is_empty() {
        Err("missing file name".into())
    } else {
        Ok(p.to_string())
    }
}

fn reverb(params: &str) -> Result<ReverbSpec, String> {
    let toks: Vec<&str> = params.split_whitespace().collect();
    if toks.len() == 1 && toks[0].eq_ignore_ascii_case("off") {
        return Ok(ReverbSpec {
            mix: 0.0,
            ..ReverbSpec::default()
        });
    }
    let mut spec = ReverbSpec::default();
    let mut i = 0;
    while i < toks.len() {
        let key = toks[i].to_ascii_lowercase();
        let raw = toks.get(i + 1).ok_or(format!("missing value after {}", toks[i]))?;
        i += 2;
        let (v, percent) = match raw.strip_suffix('%') {
            Some(n) => (num(n)?, true),
            None => {
                let percent = toks.get(i).is_some_and(|t| *t == "%");
                if percent {
                    i += 1;
                }
                (num(raw)?, percent)
            }
        };
        let v = if percent { v / 100.0 } else { v };
        if !(0.0..=1.0).contains(&v) {
            return Err(format!("{key} must be between 0 and 1 (or 0 and 100 %)"));
        }
        match key.as_str() {
            "mix" | "wet" => spec.mix = v,
            "room" | "size" => spec.room = v,
            "damping" | "damp" => spec.damping = v,
            _ => return Err(format!("unknown reverb setting {key:?}")),
        }
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmds(text: &str) -> Vec<Command> {
        parse(text).commands.into_iter().map(|(_, c)| c).collect()
    }

    fn biquad(text: &str) -> BiquadSpec {
        match cmds(text).as_slice() {
            [Command::Filter(Filter::Biquad(b))] => *b,
            other => panic!("{text}: {other:?}"),
        }
    }

    #[test]
    fn reference_examples_parse() {
        assert_eq!(cmds("Preamp: -6.5 dB"), vec![Command::Preamp(-6.5)]);
        assert_eq!(cmds("Preamp: -6 db"), vec![Command::Preamp(-6.0)]);
        let b = biquad("Filter  1: ON  PK       Fc     50 Hz   Gain  -3.0 dB  Q 10.00");
        assert_eq!((b.kind, b.fc, b.gain_db, b.q), (Kind::Peaking, 50.0, -3.0, Some(10.0)));
        let b = biquad("Filter: ON PEQ Fc 100 Hz Gain 1.0 dB BW Oct 0.167");
        assert_eq!((b.kind, b.bw_oct), (Kind::Peaking, Some(0.167)));
        let b = biquad("Filter: ON Modal Fc 100 Hz Gain 3.0 dB Q 5.41 T60 target 100 ms");
        assert_eq!((b.kind, b.q), (Kind::Peaking, Some(5.41)));
        assert_eq!(biquad("Filter: ON LP Fc 8000 Hz").kind, Kind::LowPass);
        assert_eq!(biquad("Filter: ON LPQ Fc 10000 Hz Q 0.400").q, Some(0.4));
        assert_eq!(biquad("Filter: ON HP Fc 30.0 Hz").kind, Kind::HighPass);
        assert_eq!(biquad("Filter: ON BP Fc 1000 Hz Q 0.100").kind, Kind::BandPass);
        assert_eq!(biquad("Filter: ON NO Fc 800 Hz").kind, Kind::Notch);
        assert_eq!(biquad("Filter: ON AP Fc 900 Hz Q 0.707").kind, Kind::AllPass);
        assert_eq!(biquad("Filter: ON LS Fc 300 Hz Gain 5.0 dB").kind, Kind::LowShelf);
        let b = biquad("Filter: ON LSC 10.8 dB Fc 300 Hz Gain 5.0 dB");
        assert_eq!((b.kind, b.slope_db), (Kind::LowShelf, Some(10.8)));
        assert_eq!(biquad("Filter: ON LSC Fc 300 Hz Gain 5.0 dB Q 0.6473").q, Some(0.6473));
        assert_eq!(biquad("Filter: ON HSC 6 dB Fc 100 Hz Gain -6.0 dB").slope_db, Some(6.0));
        assert_eq!(biquad("Filter: ON LS 6dB Fc 50.0 Hz Gain 7.2 dB").kind, Kind::LowShelf6);
        assert_eq!(
            biquad("Filter: ON LS 12dB Fc 2000 Hz Gain -5.0 dB").kind,
            Kind::LowShelf12
        );
        assert_eq!(
            biquad("Filter: ON HS 6dB Fc 12000 Hz Gain 10.0 dB").kind,
            Kind::HighShelf6
        );
        assert_eq!(
            biquad("Filter: ON HS 12dB Fc 500 Hz Gain 5.0 dB").kind,
            Kind::HighShelf12
        );
        assert_eq!(biquad("Filter: ON HS Fc 1000 Hz Gain -3.0 dB").kind, Kind::HighShelf);
    }

    #[test]
    fn iir_graphic_eq_channel_delay() {
        let c = cmds("Filter: ON IIR Order 2 Coefficients 0.0380602 0.0761205 0.0380602 1.2706 -1.84776 0.729402");
        let Command::Filter(Filter::Iir { b, a }) = &c[0] else {
            panic!()
        };
        assert_eq!(a[0], 1.0);
        assert!((b[0] - 0.0380602 / 1.2706).abs() < 1e-12);
        assert_eq!(
            cmds("GraphicEQ: 40 4.5; 25 6;63 3;"),
            vec![Command::GraphicEq(vec![(25.0, 6.0), (40.0, 4.5), (63.0, 3.0)])]
        );
        assert_eq!(cmds("Channel: L"), vec![Command::Channel([true, false])]);
        assert_eq!(cmds("Channel: 2"), vec![Command::Channel([false, true])]);
        assert_eq!(cmds("Channel: all"), vec![Command::Channel(ALL)]);
        let p = parse("Channel: L RL");
        assert_eq!(p.commands[0].1, Command::Channel([true, false]));
        assert_eq!(p.warnings.len(), 1);
        assert_eq!(cmds("Delay: 50.5 ms"), vec![Command::Delay(Delay::Ms(50.5))]);
        assert_eq!(cmds("Delay: 480 samples"), vec![Command::Delay(Delay::Samples(480.0))]);
    }

    #[test]
    fn copy_expressions() {
        let c = cmds("Copy: L=L+0.5*R R=-6dB*L 1=R L=0.5 R=2");
        let Command::Copy(a) = &c[0] else { panic!() };
        assert_eq!(a.len(), 5);
        assert_eq!(
            a[0].terms,
            vec![
                Term::Channel { factor: 1.0, source: 0 },
                Term::Channel { factor: 0.5, source: 1 }
            ]
        );
        let Term::Channel { factor, source: 0 } = a[1].terms[0] else {
            panic!()
        };
        assert!((factor - 0.501187).abs() < 1e-5);
        assert_eq!(a[2].target, 0);
        assert_eq!(a[3].terms, vec![Term::Constant(0.5)]);
        // "2" without a decimal point is channel 2, not a constant
        assert_eq!(a[4].terms, vec![Term::Channel { factor: 1.0, source: 1 }]);
        let p = parse("Copy: L=R+-6dB*C");
        let Command::Copy(a) = &p.commands[0].1 else { panic!() };
        assert_eq!(a[0].terms.len(), 1, "C is silence in stereo");
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn comments_unknown_and_malformed_lines() {
        let p = parse(
            "# comment\n\
             Preamp: -3 dB # trailing\n\
             not a command\n\
             Device: Speakers; Benchmark\n\
             Foo: bar\n\
             Filter: ON PK Fc 100 Hz Q 1\n\
             Filter: OFF PK Fc 100 Hz Gain 3 dB Q 1\n\
             Filter: ON PK Fc 1000 Hz Gain `20*log10(2)` dB Q 10.0\n",
        );
        assert_eq!(p.commands, vec![(2, Command::Preamp(-3.0))]);
        assert_eq!(p.warnings.len(), 4, "{:?}", p.warnings);
        assert!(p.warnings[2].contains("missing Gain"));
    }

    #[test]
    fn if_blocks_are_skipped() {
        let p = parse(
            "If: sampleRate == 44100\n  Preamp: -1 dB\n  If: x\n  Preamp: -2 dB\n  EndIf:\nElse:\n  Preamp: -3 dB\nEndIf:\nPreamp: -4 dB",
        );
        assert_eq!(p.commands, vec![(9, Command::Preamp(-4.0))]);
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn include_convolution_reverb() {
        assert_eq!(
            cmds("Include: sub dir/eq.txt\nConvolution: church.wav"),
            vec![
                Command::Include("sub dir/eq.txt".into()),
                Command::Convolution("church.wav".into())
            ]
        );
        assert_eq!(
            cmds("Reverb: Mix 30 % Room 70% Damping 0.5"),
            vec![Command::Reverb(ReverbSpec {
                mix: 0.3,
                room: 0.7,
                damping: 0.5
            })]
        );
        assert_eq!(
            cmds("Reverb: off"),
            vec![Command::Reverb(ReverbSpec {
                mix: 0.0,
                ..Default::default()
            })]
        );
        assert!(parse("Reverb: Mix 150 %").commands.is_empty());
    }

    #[test]
    fn rejects_out_of_range_values() {
        for line in [
            "Filter: ON PK Fc -5 Hz Gain 3 dB Q 1",
            "Filter: ON PK Fc 100 Hz Gain 300 dB Q 1",
            "Filter: ON PK Fc 100 Hz Gain 3 dB Q 0",
            "Filter: ON ZZ Fc 100 Hz",
            "Filter: ON IIR Order 2 Coefficients 1 2 3",
            "GraphicEQ: 0 3",
            "Delay: -1 ms",
            "Delay: 5 parsecs",
        ] {
            let p = parse(line);
            assert!(p.commands.is_empty(), "{line}");
            assert_eq!(p.warnings.len(), 1, "{line}");
        }
    }
}

//! `livecheck`: checks the real source path against the live services (port
//! of Cadence's `cmd/livecheck`, which was Deezer-specific; Wavyzon's checks
//! go through whatever sources the environment configures).
//!
//! ```text
//! livecheck                     search, open and decode one track; check it is music
//! livecheck "<query>"           the same for that query
//! livecheck -bench q1 q2 …      search / open / first-audio latency per query
//! livecheck -seek ["<query>"]   seeks vs a full decode (bit-exact after warm-up)
//! livecheck -rec "<query>"      recommendations for the first result
//! livecheck -lyrics "<artist> - <title>" [seconds]   lyrics through the provider chain (WAVYZON_LYRICS_ORDER)
//! ```
//!
//! Reads the same environment as the node (`WAVYZON_PASSWORD` included).

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use tokio_util::io::SyncIoBridge;
use wavyzon::Config;
use wavyzon_plugin::{LoadType, Stream, Track};
use wavyzon_sources::Sources;

const DEFAULT_QUERY: &str = "daft punk one more time";

async fn sources(cfg: &Config) -> anyhow::Result<Sources> {
    let tmp = std::env::temp_dir().join("wavyzon-livecheck");
    let loaded = wavyzon::plugins::load(
        wavyzon::plugins::compiled_in(),
        &cfg.plugins,
        &tmp,
        wavyzon::api::VERSION,
    )
    .await?;
    Ok(Sources::builder(&cfg.sources)?
        .plugins(loaded.registry.sources().iter().cloned())
        .lyrics_providers(loaded.registry.lyrics_providers().iter().cloned())
        .build())
}

async fn first(s: &Sources, query: &str) -> anyhow::Result<Track> {
    let res = s.load(query).await;
    match res.kind {
        LoadType::Error => bail!(
            "load {query:?}: {}",
            res.error.map(|e| e.to_string()).unwrap_or_default()
        ),
        _ => res
            .tracks
            .into_iter()
            .next()
            .with_context(|| format!("load {query:?} returned nothing")),
    }
}

/// Decode up to `max_samples` interleaved samples (all when `None`).
async fn decode(stream: Stream, max_samples: Option<usize>) -> anyhow::Result<(Vec<i16>, u32)> {
    let format = stream.format.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<i16>, u32)> {
        let reader = SyncIoBridge::new(stream.body);
        let mut pcm = wavyzon_audio::codec::decode(&format, Box::new(reader))?;
        let rate = pcm.sample_rate();
        let mut out = Vec::new();
        let mut buf = vec![0i16; 4096];
        loop {
            let n = pcm
                .read(&mut buf)
                .with_context(|| format!("read after {} samples", out.len()))?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if max_samples.is_some_and(|m| out.len() >= m) {
                break;
            }
        }
        Ok((out, rate))
    })
    .await?
}

async fn check(s: &Sources, query: &str) -> anyhow::Result<()> {
    let t = first(s, query).await?;
    println!(
        "track:    {} - {} ({} {}, {}s)",
        t.author,
        t.title,
        t.source,
        t.identifier,
        t.duration_ms / 1000
    );
    let start = Instant::now();
    let playable = s.playable(&t).await.context("mirror")?;
    if playable != t {
        println!(
            "playable: {} - {} ({} {})",
            playable.author, playable.title, playable.source, playable.identifier
        );
    }
    let stream = s.open(&playable, 0).await.context("open")?;
    let (pcm, rate) = decode(stream, Some(2 * 48_000 * 60)).await?;
    if pcm.is_empty() {
        bail!("decoded no audio");
    }
    let (mut sum, mut peak, mut jump, mut prev) = (0f64, 0f64, 0f64, 0f64);
    for &x in &pcm {
        let v = x as f64;
        sum += v * v;
        peak = peak.max(v.abs());
        jump += (v - prev).abs();
        prev = v;
    }
    let n = pcm.len() as f64;
    let rms = (sum / n).sqrt() / 32768.0;
    let smooth = jump / n / 32768.0;
    println!(
        "decoded:  {:.1}s of audio at {rate} Hz in {:?}",
        n / 2.0 / rate as f64,
        start.elapsed()
    );
    println!(
        "level:    rms {rms:.3}, peak {:.3}, mean step {smooth:.4}",
        peak / 32768.0
    );
    if rms < 0.01 {
        bail!("audio is near silent");
    }
    if smooth > 0.25 {
        bail!("audio looks like noise");
    }
    println!("result:   real music decoded; the source path works");
    Ok(())
}

async fn bench(s: &Sources, queries: &[String]) -> anyhow::Result<()> {
    println!("query\tsearch_ms\topen_ms\tfirst_audio_ms\ttotal_ms");
    for q in queries {
        let t0 = Instant::now();
        let t = first(s, q).await?;
        let t1 = Instant::now();
        let stream = s.open(&t, 0).await.with_context(|| format!("open {q:?}"))?;
        let t2 = Instant::now();
        decode(stream, Some(1920))
            .await
            .with_context(|| format!("decode {q:?}"))?;
        let t3 = Instant::now();
        let ms = |d: Duration| d.as_millis();
        println!(
            "{q}\t{}\t{}\t{}\t{}",
            ms(t1 - t0),
            ms(t2 - t1),
            ms(t3 - t2),
            ms(t3 - t0)
        );
    }
    Ok(())
}

async fn seek(s: &Sources, query: &str) -> anyhow::Result<()> {
    let t = s.playable(&first(s, query).await?).await?;
    let (full, _) = decode(s.open(&t, 0).await?, None).await?;
    println!(
        "track:    {} - {}, {} samples decoded in full",
        t.author,
        t.title,
        full.len() / 2
    );
    for at in [15_000i64, 61_234, 180_000] {
        let start = Instant::now();
        let stream = s.open(&t, at).await?;
        let start_sample = stream.start_sample;
        let (got, rate) = decode(stream, Some(2 * 48_000 * 5)).await?;
        let took = start.elapsed();
        if start_sample == 0 {
            println!("seek {at:6}ms: the source ignored the seek, the player skips forward itself ({took:?})");
            continue;
        }
        // `start_sample` is in 48 kHz units (the player's clock); `full` is
        // interleaved samples at the stream's own rate.
        let off = (start_sample * rate as i64 / 48_000) as usize * 2;
        let skip = 2 * 1152 * 4;
        let want = &got[skip..];
        if want.len() < 2 * 1024 {
            bail!("seek {at}ms: decoded too little audio to compare");
        }
        // A full decode trims the encoder delay (gapless), while a stream that
        // starts mid-file has no header to trim, so the two decodes may be
        // shifted by a constant number of samples. Locate the match within a
        // small window, then require bit-exactness to the end.
        let probe = 2 * 1024;
        let around = off + skip;
        let lo = around.saturating_sub(2 * 4096);
        let hi = (around + 2 * 4096).min(full.len().saturating_sub(want.len()));
        let found = (lo..hi).find(|&j| full[j..j + probe] == want[..probe]);
        let j = found
            .with_context(|| format!("seek {at}ms: no alignment with the full decode after the warm-up frames"))?;
        if full[j..j + want.len()] != want[..] {
            let i = (0..want.len()).find(|&i| full[j + i] != want[i]).unwrap_or(0);
            bail!("seek {at}ms: sample {} differs from the full decode", skip / 2 + i / 2);
        }
        println!(
            "seek {at:6}ms: start sample {start_sample}, {} samples bit-exact after the warm-up frames (decoder trim {:+} samples), opened in {took:?}",
            want.len() / 2,
            (j as i64 - around as i64) / 2
        );
    }
    println!("result:   seeks match the full decode");
    Ok(())
}

async fn recommend(s: &Sources, query: &str) -> anyhow::Result<()> {
    let t = first(s, query).await?;
    let start = Instant::now();
    let tracks = s.recommend(&t, 50).await.context("recommend")?;
    println!(
        "{} recommendations for {} - {} in {:?}",
        tracks.len(),
        t.author,
        t.title,
        start.elapsed()
    );
    for (i, r) in tracks.iter().enumerate() {
        println!(
            "{:2}. {} - {} ({} {})",
            i + 1,
            r.author,
            r.title,
            r.source,
            r.identifier
        );
    }
    Ok(())
}

/// Each provider on its own, then the chain the node serves.
async fn lyrics(s: &Sources, query: &str, seconds: Option<&str>) -> anyhow::Result<()> {
    let (author, title) = query.split_once(" - ").context("expected \"<artist> - <title>\"")?;
    let track = Track {
        author: author.trim().into(),
        title: title.trim().into(),
        // LRCLIB matches synced lyrics on the duration.
        duration_ms: seconds.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0) * 1000,
        ..Default::default()
    };
    if !s.lyrics_enabled() {
        bail!("no lyrics provider is enabled");
    }
    println!("providers: {}", s.lyrics_providers().join(" → "));
    let start = Instant::now();
    let got = s.lyrics(&track).await.context("lyrics")?;
    let Some(l) = got else {
        bail!("no lyrics for {author} - {title} ({:?})", start.elapsed());
    };
    let lines: Vec<&str> = l.text.lines().collect();
    println!(
        "{} lines ({} timed) from {} in {:?}:",
        lines.len(),
        l.lines.len(),
        l.provider,
        start.elapsed()
    );
    for t in l.lines.iter().take(4) {
        println!("  [{:>6} ms +{:>5}] {}", t.timestamp_ms, t.duration_ms, t.line);
    }
    for line in lines.iter().take(12) {
        println!("  {line}");
    }
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    let s = sources(&cfg).await?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = |i: usize| args.get(i).cloned().unwrap_or_else(|| DEFAULT_QUERY.to_string());
    tokio::time::timeout(Duration::from_secs(300), async {
        match args.first().map(String::as_str) {
            Some("-bench") => bench(&s, &args[1..]).await,
            Some("-seek") => seek(&s, &rest(1)).await,
            Some("-rec") => recommend(&s, &rest(1)).await,
            Some("-lyrics") => lyrics(&s, &rest(1), args.get(2).map(String::as_str)).await,
            Some(q) => check(&s, q).await,
            None => check(&s, DEFAULT_QUERY).await,
        }
    })
    .await
    .context("livecheck timed out")?
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter("warn")
        .with_writer(std::io::stderr)
        .init();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(run());
    rt.shutdown_timeout(Duration::from_secs(1));
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("livecheck failed: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

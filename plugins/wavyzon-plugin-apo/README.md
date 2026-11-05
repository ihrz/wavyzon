# wavyzon-plugin-apo

Plays Wavyzon's output through [Equalizer APO](https://sourceforge.net/projects/equalizerapo/)
configuration files: AutoEQ headphone corrections, REW exports, hand-written
parametric or graphic EQ, convolution reverbs.

```json
{ "filters": { "pluginFilters": { "apo": { "preset": "headphones/hd600" } } } }
{ "filters": { "pluginFilters": { "apo": { "config": "Preamp: -6 dB\nFilter: ON PK Fc 100 Hz Gain 4 dB Q 1.41" } } } }
```

Drop `.txt` presets and their impulse responses (`.wav`, `.flac`, `.ogg`) in
`WAVYZON_PLUGIN_APO_DIR` (default `$WAVYZON_DATA_DIR/plugins/apo`), then
`POST /v1/plugins/apo/reload`. `GET /v1/plugins/apo/` lists what loaded and
what was skipped; `POST /v1/plugins/apo/check` validates a config.

Supported commands, limits and routes: [Filters → Equalizer APO](../../docs/src/filters.md#equalizer-apo-configurations-apo).
`Reverb: Mix 30 % Room 70 % Damping 50 %` is a Wavyzon extension; APO ignores
it, so presets stay usable in APO.

[`examples/`](examples) has reverb presets to copy into the presets
directory (they are not loaded from there).

| Module | |
| --- | --- |
| `parse.rs` | APO text → commands and warnings |
| `design.rs` | RBJ biquads for every filter type, minimum-phase GraphicEQ FIR |
| `conv.rs` | Partitioned FFT convolution, one 20 ms partition per frame (no latency) |
| `ir.rs` | Impulse response decoding (symphonia) and resampling |
| `program.rs` | Compile commands (includes, impulses) into stages; the per-deck `Engine` |
| `reverb.rs` | Freeverb for `Reverb:` |
| `presets.rs` | The presets directory, loaded off the audio path |

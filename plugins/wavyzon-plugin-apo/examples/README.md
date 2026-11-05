# APO examples

Ready-made presets for the `apo` filter. They are **not loaded** from here:
the plugin only reads its presets directory (`WAVYZON_PLUGIN_APO_DIR`,
default `$WAVYZON_DATA_DIR/plugins/apo`). Copy the files you want there, then
restart the node or `POST /v1/plugins/apo/reload`.

| File | What it does |
| --- | --- |
| `reverb-large.txt` + `reverb-large-1.4s.wav` | A large-room reverb by convolution: decay 1.40 s (RT60), pre-delay 0 ms, high frequencies damped above 5 kHz, bass cut 298 Hz and treble cut 5 kHz on the reverb only, dry 0 dB, wet −11.8 dB. The WAV (48 kHz stereo, 24-bit, 2.2 s) holds both the dry path and the tail. |
| `reverb-light.txt` | The same idea with no file to upload: `Reverb: Mix 14 % Room 56 % Damping 100 %` (similar decay and level, without the cuts on the reverb). |
| `make_reverb_ir.py` | Regenerates `reverb-large-1.4s.wav`; edit the constants at the top (decay, wet level, cut frequencies) for another room. Standard library only: `python3 make_reverb_ir.py out.wav`. |

Use one as a preset:

```json
{ "filters": { "pluginFilters": { "apo": { "preset": "reverb-large" } } } }
```

or put its lines in your own configuration (for example after an EQ). Both
`.txt` files are plain Equalizer APO configurations; the WAV also works in
Equalizer APO on Windows when the device runs at 48 kHz.

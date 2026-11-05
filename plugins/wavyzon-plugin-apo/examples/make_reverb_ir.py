"""Generate a stereo reverb impulse response (48 kHz, 24-bit WAV) matching:
room Large, decay 1.40 s, pre-delay 0 ms, HF damping 5 kHz, bass cut 298 Hz,
treble cut 5 kHz (on the wet signal), dry 0 dB, wet -11.8 dB.

The IR contains the dry path (a unit impulse at t=0) plus the wet tail, so
one `Convolution:` line reproduces dry + wet. Pure standard library.

usage: python3 -I make_reverb_ir.py <out.wav>
"""
import math
import random
import struct
import sys

FS = 48_000
RT60 = 1.40          # s: decay to -60 dB
WET_DB = -11.8       # wet level relative to dry, for a broadband signal
DRY = 1.0            # 0 dB
BASS_CUT = 298.0     # Hz, high-pass on the wet signal
TREBLE_CUT = 5000.0  # Hz, low-pass on the wet signal
HF_DAMP = 5000.0     # Hz, above it the tail decays faster
LENGTH = 2.2         # s: past -90 dB
FADE_IN = 0.004      # s: soft onset of the tail (pre-delay 0, no click)


def biquad(kind, fc, q=0.7071):
    w0 = 2 * math.pi * fc / FS
    cos, sin = math.cos(w0), math.sin(w0)
    alpha = sin / (2 * q)
    if kind == "lp":
        b = [(1 - cos) / 2, 1 - cos, (1 - cos) / 2]
    else:
        b = [(1 + cos) / 2, -(1 + cos), (1 + cos) / 2]
    a = [1 + alpha, -2 * cos, 1 - alpha]
    return [v / a[0] for v in b], [1.0, a[1] / a[0], a[2] / a[0]]


def run(x, coefs):
    (b0, b1, b2), (_, a1, a2) = coefs
    z1 = z2 = 0.0
    out = []
    for v in x:
        y = b0 * v + z1
        z1 = b1 * v - a1 * y + z2
        z2 = b2 * v - a2 * y
        out.append(y)
    return out


def tail(seed):
    rng = random.Random(seed)
    n = int(LENGTH * FS)
    fade = int(FADE_IN * FS)
    # Split the noise at HF_DAMP: the high band decays twice as fast, the
    # usual "HF damping" of an algorithmic reverb.
    noise = [rng.gauss(0.0, 1.0) for _ in range(n)]
    low = run(noise, biquad("lp", HF_DAMP))
    high = [v - l for v, l in zip(noise, low)]
    k = 3 * math.log(10) / RT60  # amplitude decay rate for -60 dB at RT60
    sig = []
    for i in range(n):
        t = i / FS
        env = math.exp(-k * t)
        env_hi = math.exp(-2 * k * t)
        onset = min(1.0, i / fade) if fade else 1.0
        sig.append((low[i] * env + high[i] * env_hi) * onset)
    # Bass cut and treble cut of the wet path (12 dB/oct each, cascaded twice
    # for 24 dB/oct like most reverb plugins' cut filters).
    for _ in range(2):
        sig = run(sig, biquad("hp", BASS_CUT))
        sig = run(sig, biquad("lp", TREBLE_CUT))
    # Scale the tail so its broadband (white noise) gain is WET_DB.
    energy = math.sqrt(sum(v * v for v in sig))
    scale = 10 ** (WET_DB / 20) / energy
    return [v * scale for v in sig]


def write_wav24(path, channels):
    n = len(channels[0])
    ch = len(channels)
    data = bytearray()
    for i in range(n):
        for c in channels:
            v = max(-1.0, min(1.0, c[i]))
            iv = int(round(v * 8_388_607))
            data += struct.pack("<i", iv)[:3]
    fmt = struct.pack("<HHIIHH", 1, ch, FS, FS * ch * 3, ch * 3, 24)
    with open(path, "wb") as f:
        f.write(b"RIFF" + struct.pack("<I", 4 + 8 + len(fmt) + 8 + len(data)) + b"WAVE")
        f.write(b"fmt " + struct.pack("<I", len(fmt)) + fmt)
        f.write(b"data" + struct.pack("<I", len(data)) + data)


left, right = tail(1), tail(2)  # decorrelated: a wide stereo tail
left[0] += DRY
right[0] += DRY
write_wav24(sys.argv[1], [left, right])
wet_l = math.sqrt(sum(v * v for v in left[1:]))
peak_tail = max(abs(v) for v in left[1:] + right[1:])
print(f"wrote {sys.argv[1]}: {len(left)} frames, wet gain {20 * math.log10(wet_l):.1f} dB, tail peak {peak_tail:.4f}")

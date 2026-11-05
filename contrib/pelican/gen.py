"""Generate the Wavyzon eggs from install.sh and icon.png.

    python3 contrib/pelican/gen.py contrib/pelican

writes, next to this script:
  egg-wavyzon.yaml               Pelican (PLCN_v3), the layout the panel exports
  egg-pterodactyl-wavyzon.json   Pterodactyl (PTDL_v2)

Both are the files submitted to pelican-eggs/software (wavyzon/). No third-party
module: the YAML emitter below only handles the shapes an egg uses.
"""
import base64
import json
import os
import re
import sys
import uuid

here = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(os.path.abspath(__file__))
script = open(os.path.join(here, "install.sh"), encoding="utf-8").read()
icon = "data:image/png;base64," + base64.b64encode(open(os.path.join(here, "icon.png"), "rb").read()).decode()

EXPORTED_AT = "2026-11-05T12:00:00+00:00"
AUTHOR = "anais.saraiva@ihorizon.org"
UPDATE_URL = "https://raw.githubusercontent.com/pelican-eggs/software/refs/heads/main/wavyzon/egg-wavyzon.yaml"
DESCRIPTION = (
    "Standalone audio node for Discord bots, written in Rust: frame-accurate timing (wire clock), "
    "two-deck mixing and crossfades, pure-Rust DAVE end-to-end encryption, and Lavalink v4 compatibility "
    "so existing Lavalink clients work unchanged.\n"
    "Built from source at install time: https://gitlab.com/ihrz/wavyzon"
)
STARTUP = "WAVYZON_ADDRESS=0.0.0.0:{{SERVER_PORT}} ./wavyzon"
DONE = "wavyzon listening"
IMAGES = {
    "Debian": "ghcr.io/pelican-eggs/yolks:debian",
    "Ubuntu": "ghcr.io/pelican-eggs/yolks:ubuntu",
}
INSTALL_CONTAINER = "ghcr.io/pelican-eggs/installers:debian"


def var(name, env, default, desc, rules, editable=True, viewable=True):
    return {
        "name": name,
        "description": desc,
        "env_variable": env,
        "default_value": default,
        "user_viewable": viewable,
        "user_editable": editable,
        "rules": rules,
    }


BOOL = ["required", "string", "in:true,false"]
variables = [
    var("Password", "WAVYZON_PASSWORD", "",
        "Shared secret required on every request (Authorization header). Required; 'youshallnotpass' is refused.",
        ["required", "string", "min:8", "not_in:youshallnotpass"]),
    var("Git repository", "WAVYZON_REPO", "https://gitlab.com/ihrz/wavyzon.git",
        "Repository built at install time.", ["required", "string", "max:200"], editable=False),
    var("Branch or tag", "WAVYZON_REF", "main",
        "Branch or tag to build. Reinstall the server to update.", ["required", "string", "max:100"]),
    var("Git token (private fork)", "WAVYZON_GIT_TOKEN", "",
        "Optional: the repository is public. GitLab access token (read_repository) only for a private fork.",
        ["nullable", "string", "max:200"], editable=False, viewable=False),
    var("Build jobs", "WAVYZON_BUILD_JOBS", "2",
        "Parallel compile jobs during install (~1.5 GB of RAM each). Lower it if the install runs out of memory.",
        ["required", "integer", "between:1,32"], editable=False),
    var("Log level", "WAVYZON_LOG_LEVEL", "info", "debug, info, warn or error.",
        ["required", "string", "in:debug,info,warn,error"]),
    var("Log format", "WAVYZON_LOG_FORMAT", "pretty",
        "pretty (readable in the console) or json (one JSON line per event, for a log collector).",
        ["required", "string", "in:pretty,json"]),
    var("Buffer per deck (ms)", "WAVYZON_BUFFER_MS", "10000",
        "Decoded audio buffered per deck (1000-60000).", ["required", "integer", "between:1000,60000"]),
    var("Default Opus bitrate", "WAVYZON_BITRATE", "96000",
        "Opus bitrate when the bot sends none (bps, 16000-384000).", ["required", "integer", "between:16000,384000"]),
    var("Playout delay (ms)", "WAVYZON_PLAYOUT_DELAY_MS", "0",
        "Measured delay between Discord receiving audio and listeners hearing it, added to audibleAt.",
        ["required", "integer", "min:0"]),
    var("Pause/resume fade (ms)", "WAVYZON_PAUSE_FADE_MS", "0",
        "0 = instant pause (0-2000).", ["required", "integer", "between:0,2000"]),
    var("Resume timeout (s)", "WAVYZON_RESUME_TIMEOUT_SECONDS", "300",
        "How long players survive while their client is disconnected.", ["required", "integer", "min:1"]),
    var("playerUpdate interval (ms)", "WAVYZON_UPDATE_INTERVAL_MS", "1000",
        "How often playerUpdate is sent (min 250).", ["required", "integer", "min:250"]),
    var("Search order", "WAVYZON_SEARCH_ORDER", "deezer,applemusic,spotify,soundcloud",
        "Sources queried for a text search, in order.", ["required", "string", "max:200"]),
    var("Playback sources", "WAVYZON_PLAYBACK_SOURCES", "deezer,soundcloud",
        "Sources that actually play audio; tracks from other sources are matched to them by ISRC or title.",
        ["required", "string", "max:200"]),
    var("Prefetch on loadtracks", "WAVYZON_PREFETCH_ON_LOAD", "true",
        "Preload the first result of each search so playback starts instantly.", BOOL),
    var("Analysis workers", "WAVYZON_ANALYSIS_WORKERS", "2",
        "Offline analysis concurrency (1-64).", ["required", "integer", "between:1,64"]),
    var("Analysis queue", "WAVYZON_ANALYSIS_QUEUE", "32",
        "Maximum size of the analysis queue.", ["required", "integer", "min:1"]),
    var("Audio cache directory", "WAVYZON_AUDIO_CACHE_DIR", "/home/container/cache",
        "Disk cache directory (empty = disabled).", ["nullable", "string", "max:200"]),
    var("Audio cache size (GB)", "WAVYZON_AUDIO_CACHE_GB", "5",
        "Disk cache limit (1-1000).", ["required", "integer", "between:1,1000"]),
    var("Loudness normalization", "WAVYZON_NORMALIZE", "true",
        "Bring every track to the same loudness.", BOOL),
    var("Target LUFS", "WAVYZON_NORMALIZE_LUFS", "-14",
        "Loudness target (-30 to -5).", ["required", "integer", "between:-30,-5"]),
    var("State file", "WAVYZON_STATE_FILE", "/home/container/state.json",
        "Sessions are saved here on shutdown and resumed after a restart (empty = disabled).",
        ["nullable", "string", "max:200"]),
    var("HTTP source", "WAVYZON_HTTP_SOURCE", "true",
        "Play direct audio links (MP3, FLAC...) and Icecast/Shoutcast radios.", BOOL),
    var("HTTP: private addresses", "WAVYZON_HTTP_ALLOW_PRIVATE", "false",
        "Let the HTTP source reach the local network (10.x, 192.168.x, 127.0.0.1...). Keep false unless every client is trusted.",
        BOOL),
    var("Local files directory", "WAVYZON_LOCAL_ROOT", "",
        "Directory local tracks may be played from, e.g. /home/container/music (empty = disabled).",
        ["nullable", "string", "max:200"]),
    var("Plugins", "WAVYZON_PLUGINS", "",
        "Comma-separated plugins to enable (empty = every compiled-in plugin).", ["nullable", "string", "max:200"]),
    var("Deezer enabled", "DEEZER_ENABLED", "true",
        "Deezer search, links and playback.", BOOL),
    var("Deezer ARL", "DEEZER_ARL", "",
        "Comma-separated Deezer account ARL cookies. Without them Deezer is metadata only.",
        ["nullable", "string", "max:500"]),
    var("Deezer ARL rotation", "DEEZER_ARL_ROTATION", "failover",
        "failover, round-robin or random when several ARLs are set.",
        ["required", "string", "in:failover,round-robin,random"]),
    var("Deezer master key", "DEEZER_MASTER_KEY", "",
        "Deezer master key. Required with DEEZER_ARL for Deezer playback.",
        ["nullable", "string", "max:64"]),
    var("Deezer high quality", "DEEZER_HIGH_QUALITY", "true",
        "Ask for 128 kbit/s MP3 (falling back to 64) instead of 64 only.", BOOL),
    var("Spotify client ID", "SPOTIFY_CLIENT_ID", "",
        "Spotify Web API credentials (metadata only). Without them Spotify links don't resolve.",
        ["nullable", "string", "max:100"]),
    var("Spotify client secret", "SPOTIFY_CLIENT_SECRET", "",
        "Spotify Web API secret.", ["nullable", "string", "max:100"]),
    var("Apple Music storefront", "APPLE_MUSIC_STOREFRONT", "us",
        "Apple Music catalogue storefront.", ["required", "string", "max:10"]),
    var("SoundCloud enabled", "SOUNDCLOUD_ENABLED", "true",
        "SoundCloud search and playback.", BOOL),
    var("Cache peers", "WAVYZON_CACHE_PEERS", "",
        "Other Wavyzon nodes to share the disk cache with, comma-separated URLs (empty = disabled).",
        ["nullable", "string", "max:500"]),
    var("Shared cache secret", "WAVYZON_CACHE_SECRET", "",
        "Same secret on every node; required by WAVYZON_CACHE_PEERS.", ["nullable", "string", "max:200"]),
    var("Automatic crossfade (ms)", "WAVYZON_CROSSFADE_MS", "3000",
        "Fade into the queued nextTrack before the current track ends (0-12000, 0 = strict gapless).",
        ["required", "integer", "between:0,12000"]),
    var("Lyrics (lyrics.ovh)", "LYRICS_ENABLED", "true",
        "Serve lyrics from lyrics.ovh on /v4/lyrics (LavaLyrics API).", BOOL),
]
for i, v in enumerate(variables, 1):
    v["sort"] = i

egg_uuid = str(uuid.uuid5(uuid.NAMESPACE_URL, "https://gitlab.com/ihrz/wavyzon/pelican-egg"))

# ---- Pelican (PLCN_v3, YAML as the panel exports it) ------------------------------------

pelican = {
    "_comment": "DO NOT EDIT: FILE GENERATED AUTOMATICALLY BY PANEL",
    "meta": {"version": "PLCN_v3", "update_url": UPDATE_URL},
    "exported_at": EXPORTED_AT,
    "name": "Wavyzon",
    "author": AUTHOR,
    "uuid": egg_uuid,
    "description": DESCRIPTION,
    "icon": icon,
    "tags": ["audio", "discord", "lavalink", "rust"],
    "features": {},
    "docker_images": IMAGES,
    "file_denylist": {},
    "startup_commands": {"Default": STARTUP},
    "config": {"files": {}, "startup": {"done": DONE}, "logs": {}, "stop": "^C"},
    "scripts": {"installation": {"script": script.rstrip("\n"), "container": INSTALL_CONTAINER, "entrypoint": "bash"}},
    "variables": [
        {k: v[k] for k in ("sort", "name", "description", "env_variable", "default_value",
                           "user_viewable", "user_editable", "rules")}
        for v in variables
    ],
}

SAFE = re.compile(r"^[A-Za-z_][A-Za-z0-9_.-]*$")
RESERVED = {"true", "false", "null", "yes", "no", "on", "off", "y", "n", "~"}


def scalar(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if v is None:
        return "null"
    if isinstance(v, int):
        return str(v)
    if v == "^C":
        return "^C"
    if SAFE.match(v) and v.lower() not in RESERVED:
        return v
    return "'" + v.replace("'", "''") + "'"


def emit(value, indent=0):
    pad = "  " * indent
    out = []
    if isinstance(value, dict):
        for k, v in value.items():
            if isinstance(v, dict) and not v:
                out.append(f"{pad}{k}: {{  }}")
            elif isinstance(v, (dict, list)) and v:
                out.append(f"{pad}{k}:")
                out.extend(emit(v, indent + 1))
            elif isinstance(v, str) and "\n" in v:
                out.append(f"{pad}{k}: |-")
                out.extend(f"{pad}  {line}" if line else "" for line in v.split("\n"))
            else:
                out.append(f"{pad}{k}: {scalar(v)}")
    else:
        for item in value:
            if isinstance(item, dict):
                out.append(f"{pad}-")
                out.extend(emit(item, indent + 1))
            else:
                out.append(f"{pad}- {scalar(item)}")
    return out


with open(os.path.join(here, "egg-wavyzon.yaml"), "w", encoding="utf-8") as f:
    f.write("\n".join(emit(pelican)) + "\n")

# ---- Pterodactyl (PTDL_v2) ---------------------------------------------------------------

pterodactyl = {
    "_comment": "DO NOT EDIT: FILE GENERATED AUTOMATICALLY BY PTERODACTYL PANEL - PTERODACTYL.IO",
    "meta": {"version": "PTDL_v2", "update_url": None},
    "exported_at": EXPORTED_AT,
    "name": "Wavyzon",
    "author": AUTHOR,
    "description": DESCRIPTION,
    "features": None,
    "docker_images": IMAGES,
    "file_denylist": [],
    "startup": STARTUP,
    "config": {
        "files": "{}",
        "startup": json.dumps({"done": DONE}, indent=4),
        "logs": "{}",
        "stop": "^C",
    },
    "scripts": {"installation": {"script": script.rstrip("\n"), "container": INSTALL_CONTAINER, "entrypoint": "bash"}},
    "variables": [
        {
            "name": v["name"],
            "description": v["description"],
            "env_variable": v["env_variable"],
            "default_value": v["default_value"],
            "user_viewable": v["user_viewable"],
            "user_editable": v["user_editable"],
            "rules": "|".join(v["rules"]),
            "field_type": "text",
        }
        for v in variables
    ],
}

with open(os.path.join(here, "egg-pterodactyl-wavyzon.json"), "w", encoding="utf-8") as f:
    json.dump(pterodactyl, f, indent=4, ensure_ascii=False)
    f.write("\n")

print(f"{len(variables)} variables")

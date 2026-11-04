"""Generate the Wavyzon Pelican egg (PLCN_v3) from install.sh."""
import json
import sys
import uuid

install_path, out_path = sys.argv[1], sys.argv[2]
script = open(install_path, encoding="utf-8").read()


def var(sort, name, env, default, desc, rules, editable=True, viewable=True):
    return {
        "name": name,
        "description": desc,
        "env_variable": env,
        "default_value": default,
        "user_viewable": viewable,
        "user_editable": editable,
        "rules": rules,
        "sort": sort,
    }


BOOL = ["required", "string", "in:true,false"]
variables = [
    var(1, "Mot de passe (WAVYZON_PASSWORD)", "WAVYZON_PASSWORD", "",
        "Secret partagé exigé sur chaque requête (header Authorization). Obligatoire, 'youshallnotpass' est refusé.",
        ["required", "string", "min:8", "not_in:youshallnotpass"]),
    var(2, "Dépôt Git", "WAVYZON_REPO", "https://gitlab.com/ihrz/wavyzon.git",
        "Dépôt source compilé à l'installation.", ["required", "string", "max:200"], editable=False),
    var(3, "Branche / Tag", "WAVYZON_REF", "main",
        "Branche ou tag à compiler (réinstaller le serveur pour mettre à jour).", ["required", "string", "max:100"]),
    var(4, "Jeton Git (dépôt privé)", "WAVYZON_GIT_TOKEN", "",
        "Facultatif : le dépôt est public. Jeton d'accès GitLab (scope read_repository) seulement pour un fork privé.",
        ["nullable", "string", "max:200"], editable=False, viewable=False),
    var(5, "Jobs de compilation", "WAVYZON_BUILD_JOBS", "2",
        "Compilations en parallèle à l'installation (~1,5 Go de RAM par job). Baisser si l'installation est tuée par manque de mémoire.",
        ["required", "integer", "between:1,32"], editable=False),
    var(6, "Niveau de logs", "WAVYZON_LOG_LEVEL", "info", "debug, info, warn ou error.",
        ["required", "string", "in:debug,info,warn,error"]),
    var(7, "Format des logs", "WAVYZON_LOG_FORMAT", "pretty",
        "pretty (lisible dans la console) ou json (une ligne JSON par événement, pour un collecteur de logs).",
        ["required", "string", "in:pretty,json"]),
    var(8, "Buffer par deck (ms)", "WAVYZON_BUFFER_MS", "10000",
        "Audio décodé mis en buffer par deck (1000-60000).", ["required", "integer", "between:1000,60000"]),
    var(9, "Bitrate Opus par défaut", "WAVYZON_BITRATE", "96000",
        "Bitrate Opus quand le bot n'en envoie pas (bps, 16000-384000).", ["required", "integer", "between:16000,384000"]),
    var(10, "Playout delay (ms)", "WAVYZON_PLAYOUT_DELAY_MS", "0",
        "Délai mesuré entre la réception par Discord et l'écoute, ajouté à audibleAt.", ["required", "integer", "min:0"]),
    var(11, "Fondu pause/reprise (ms)", "WAVYZON_PAUSE_FADE_MS", "0",
        "0 = pause instantanée (0-2000).", ["required", "integer", "between:0,2000"]),
    var(12, "Timeout de reprise (s)", "WAVYZON_RESUME_TIMEOUT_SECONDS", "300",
        "Durée de survie des players quand le client est déconnecté.", ["required", "integer", "min:1"]),
    var(13, "Intervalle playerUpdate (ms)", "WAVYZON_UPDATE_INTERVAL_MS", "1000",
        "Cadence des playerUpdate (min 250).", ["required", "integer", "min:250"]),
    var(14, "Ordre de recherche", "WAVYZON_SEARCH_ORDER", "deezer,applemusic,spotify,soundcloud",
        "Sources interrogées pour une recherche texte, dans l'ordre.",
        ["required", "string", "max:200"]),
    var(15, "Sources de lecture", "WAVYZON_PLAYBACK_SOURCES", "deezer,soundcloud",
        "Sources qui jouent vraiment l'audio ; les pistes Spotify/Apple Music y sont retrouvées par ISRC.",
        ["required", "string", "max:200"]),
    var(16, "Préchargement au loadtracks", "WAVYZON_PREFETCH_ON_LOAD", "true",
        "Précharger le premier résultat de chaque recherche pour un démarrage instantané.", BOOL),
    var(17, "Workers d'analyse", "WAVYZON_ANALYSIS_WORKERS", "2",
        "Concurrence de l'analyse hors-ligne (1-64).", ["required", "integer", "between:1,64"]),
    var(18, "File d'analyse", "WAVYZON_ANALYSIS_QUEUE", "32",
        "Taille maximale de la file d'analyse.", ["required", "integer", "min:1"]),
    var(19, "Dossier cache audio", "WAVYZON_AUDIO_CACHE_DIR", "/home/container/cache",
        "Dossier du cache disque (vide = désactivé).", ["nullable", "string", "max:200"]),
    var(20, "Taille cache audio (Go)", "WAVYZON_AUDIO_CACHE_GB", "5",
        "Plafond du cache disque (1-1000).", ["required", "integer", "between:1,1000"]),
    var(21, "Normalisation du volume", "WAVYZON_NORMALIZE", "true",
        "Aligner chaque piste sur la même loudness.", BOOL),
    var(22, "Cible LUFS", "WAVYZON_NORMALIZE_LUFS", "-14",
        "Cible de loudness (-30 à -5).", ["required", "integer", "between:-30,-5"]),
    var(23, "Fichier d'état", "WAVYZON_STATE_FILE", "/home/container/state.json",
        "Sauvegarde des sessions à l'arrêt pour reprendre après redémarrage (vide = désactivé).",
        ["nullable", "string", "max:200"]),
    var(24, "Source HTTP", "WAVYZON_HTTP_SOURCE", "true",
        "Jouer les liens audio directs (MP3, FLAC…) et les radios Icecast/Shoutcast.", BOOL),
    var(25, "HTTP : adresses privées", "WAVYZON_HTTP_ALLOW_PRIVATE", "false",
        "Autoriser la source HTTP à joindre le réseau local (10.x, 192.168.x, 127.0.0.1…). Laisser à false sauf si tous les clients sont de confiance.",
        BOOL),
    var(26, "Dossier de fichiers locaux", "WAVYZON_LOCAL_ROOT", "",
        "Dossier depuis lequel les pistes locales peuvent être jouées, ex. /home/container/music (vide = désactivé).",
        ["nullable", "string", "max:200"]),
    var(27, "Plugins", "WAVYZON_PLUGINS", "",
        "Plugins à activer, séparés par des virgules (vide = tous ceux compilés).", ["nullable", "string", "max:200"]),
    var(28, "Deezer activé", "DEEZER_ENABLED", "true",
        "Recherche, liens et lecture Deezer.", BOOL),
    var(29, "Deezer ARL", "DEEZER_ARL", "",
        "Cookies ARL de comptes Deezer, séparés par des virgules. Nécessaire à la lecture ; sans eux Deezer reste métadonnées.",
        ["nullable", "string", "max:500"]),
    var(30, "Rotation des ARL", "DEEZER_ARL_ROTATION", "failover",
        "failover, round-robin ou random quand plusieurs ARL sont configurés.",
        ["required", "string", "in:failover,round-robin,random"]),
    var(31, "Clé maître Deezer", "DEEZER_MASTER_KEY", "",
        "Clé Blowfish de 16 octets pour déchiffrer les flux Deezer. Nécessaire à la lecture.",
        ["nullable", "string", "max:64"]),
    var(32, "Deezer haute qualité", "DEEZER_HIGH_QUALITY", "true",
        "Demander du MP3 128 kbit/s (avec repli 64) au lieu de 64 seul.", BOOL),
    var(33, "Spotify Client ID", "SPOTIFY_CLIENT_ID", "",
        "Identifiants Spotify Web API (métadonnées uniquement). Sans eux, les liens Spotify ne se résolvent pas.",
        ["nullable", "string", "max:100"]),
    var(34, "Spotify Client Secret", "SPOTIFY_CLIENT_SECRET", "",
        "Secret Spotify Web API.", ["nullable", "string", "max:100"]),
    var(35, "Apple Music storefront", "APPLE_MUSIC_STOREFRONT", "us",
        "Storefront du catalogue Apple Music.", ["required", "string", "max:10"]),
    var(36, "SoundCloud activé", "SOUNDCLOUD_ENABLED", "true",
        "Lecture depuis SoundCloud (source de lecture par défaut).", BOOL),
    var(37, "Pairs du cache", "WAVYZON_CACHE_PEERS", "",
        "Autres nœuds Wavyzon avec qui partager le cache disque, URLs séparées par des virgules (vide = désactivé).",
        ["nullable", "string", "max:500"]),
    var(38, "Secret du cache partagé", "WAVYZON_CACHE_SECRET", "",
        "Secret identique sur tous les nœuds, exigé pour WAVYZON_CACHE_PEERS.", ["nullable", "string", "max:200"]),
    var(39, "Fondu enchaîné automatique (ms)", "WAVYZON_CROSSFADE_MS", "3000",
        "Fondu vers le nextTrack avant la fin du morceau, comme Tidal (0-12000, 0 = gapless strict).",
        ["required", "integer", "between:0,12000"]),
    var(40, "Paroles (lyrics.ovh)", "LYRICS_ENABLED", "true",
        "Servir les paroles depuis lyrics.ovh sur /v4/lyrics (API LavaLyrics).", BOOL),
]
variables.sort(key=lambda v: v["name"].lower())

egg = {
    "_comment": "DO NOT EDIT: FILE GENERATED AUTOMATICALLY BY PANEL",
    "meta": {"version": "PLCN_v3", "update_url": None},
    "exported_at": "2026-11-04T09:00:00+00:00",
    "name": "Wavyzon",
    "author": "admin@example.com",
    "uuid": str(uuid.uuid5(uuid.NAMESPACE_URL, "https://gitlab.com/ihrz/wavyzon/pelican-egg")),
    "description": "Wavyzon : nœud audio standalone pour bots Discord (Rust), réécriture de Cadence. Timing à la trame près (wire clock), mixage deux decks, DAVE E2EE en Rust pur et compatibilité Lavalink v4. Compilé depuis https://gitlab.com/ihrz/wavyzon à l'installation.",
    "icon": None,
    "tags": ["audio", "discord", "rust", "lavalink"],
    "features": [],
    "docker_images": {
        "Debian": "ghcr.io/pelican-eggs/yolks:debian",
        "Ubuntu": "ghcr.io/pelican-eggs/yolks:ubuntu",
    },
    "file_denylist": [],
    "startup_commands": {"Default": "bash ./start.sh"},
    "config": {
        "files": "{}",
        "startup": json.dumps({"done": "wavyzon listening"}, indent=4),
        "logs": "{}",
        "stop": "^C",
    },
    "scripts": {
        "installation": {
            "script": script,
            "container": "rust:1-bookworm",
            "entrypoint": "bash",
        }
    },
    "variables": variables,
}

with open(out_path, "w", encoding="utf-8") as f:
    json.dump(egg, f, indent=4, ensure_ascii=True)
    f.write("\n")
print(f"{len(variables)} variables")

# Version 0.2.0 (lecture Deezer, fondus enchaînés, paroles et trois nouveaux plugins)

---

## 🎧 Deezer joue de nouveau

Deezer redevient une **source de lecture** : connectez un ou plusieurs cookies de compte (`DEEZER_ARL`, rotation `failover`, `round-robin` ou `random`) et `DEEZER_MASTER_KEY`, et Wavyzon lit et déchiffre lui-même les pistes Deezer, avec des **sauts instantanés** sur les fichiers à débit constant. Sans eux, Deezer reste limité aux métadonnées et ses pistes sont jouées depuis une autre source de lecture, comme en 0.1.0. Les recommandations utilisent de nouveau le mix du compte connecté.

> **Valeurs par défaut modifiées** : `WAVYZON_PLAYBACK_SOURCES` vaut désormais `deezer,soundcloud` et `WAVYZON_SEARCH_ORDER` commence par `deezer`. Remettez `soundcloud` pour garder le comportement de la 0.1.0.

---

## 🔀 Fondus enchaînés automatiques

Quand le bot met en file un `nextTrack`, la piste en cours **se fond dans la suivante** `WAVYZON_CROSSFADE_MS` avant sa fin (3 s par défaut, à puissance constante) au lieu de basculer sur la dernière trame. Le bot reçoit les mêmes événements qu'un enchaînement sans blanc. `0` garde l'enchaînement strict sans blanc.

---

## 🎤 Paroles : Genius, LRCLIB (synchronisées) et lyrics.ovh

`/v1/lyrics` et `/v4/lyrics` servent les paroles au format du plugin **LavaLyrics** de Lavalink, et `/v4/info` annonce `lavalyrics-plugin`. Trois fournisseurs sont interrogés tour à tour : le premier qui a les paroles répond, et celui qui échoue passe la main au suivant :

- **[LRCLIB](https://lrclib.net)**, sans clé, avec des **paroles synchronisées** : des `lines` horodatées que le bot peut afficher en rythme avec la musique (`LRCLIB_ENABLED`).
- **lyrics.ovh**, sans clé, texte brut (`LYRICS_ENABLED`).
- **[Genius](https://genius.com)**, le nouveau plugin `genius`, activé par défaut. Renseignez `WAVYZON_PLUGIN_GENIUS_ACCESS_TOKEN` (ou `_CLIENT_ID` + `_CLIENT_SECRET`). Les pages de Genius refusent beaucoup de serveurs même avec une clé ; Genius est alors mis de côté 10 minutes.

`WAVYZON_LYRICS_ORDER` fixe l'ordre (par défaut `lrclib,lyricsovh,genius` ; `genius,lrclib,lyricsovh` essaie Genius en premier) ; le champ `provider` indique qui a répondu. `livecheck -lyrics "<artiste> - <titre>" [secondes]` vérifie la chaîne en direct.

---

## 🗣️ Synthèse vocale Flowery

Le nouveau plugin `flowery`, **activé par défaut**, fait parler le bot avec la synthèse vocale gratuite de [Flowery](https://flowery.pw). Il lit les identifiants `ftts://<texte>?voice=…&speed=…` de LavaSrc et annonce `ftts` et `flowery-tts` dans `/v4/info` : les recherches `source: "ftts"` de lavalink-client fonctionnent comme sur Lavalink avec LavaSrc. Aucune clé d'API ; `WAVYZON_PLUGIN_FLOWERY_ENABLED=false` le désactive.

---

## 🎚️ Configurations Equalizer APO

Le nouveau plugin `apo`, **activé par défaut**, fait passer la lecture par des configs et presets **Equalizer APO** avec `filters.pluginFilters.apo` : filtres paramétriques, GraphicEQ, convolution avec réponses impulsionnelles (sans latence ajoutée) et une extension de réverbération. Les presets vivent dans un dossier et se rechargent sans redémarrage.

---

## 🧩 API de plugins

Les plugins peuvent désormais ajouter des **fournisseurs de paroles** (`Registry::lyrics`), placés par leur nom dans `WAVYZON_LYRICS_ORDER`, en plus des sources, filtres, écouteurs d'événements et routes. `Lyrics` porte des `lines` horodatées (paroles simples : `Lyrics::plain`). `API_VERSION` reste `1`.

---

## 🛠️ Corrections

- Un flux qui se coupe en cours de piste est **rouvert là où il s'était arrêté** au lieu de terminer la piste.
- SoundCloud lit ses flux **HLS AAC**, jamais la playlist adaptative.
- La mise en miroir ne choisit plus d'**envois sans rapport ni de remixes**, et une source de type tonalité n'est jamais utilisée en repli d'une recherche texte.
- Un `play` en `noReplace` ne repositionne jamais la piste démarrée par un fondu.
- Délais d'en-têtes par tentative, délai de lecture des en-têtes côté serveur, et plusieurs écarts avec Cadence corrigés après un audit module par module.

---

## 🐳 Déploiement et documentation

- Un **egg Pelican / Pterodactyl** (`contrib/pelican/`), construit dans les images standard de pelican-eggs sans script de démarrage, qui expose chaque réglage comme variable du panel.
- Un site de documentation (mdBook) sur **wavyzon.ihorizon.org/docs/**, avec démarrage rapide, exemples, guide Pelican et référence de l'API.
- Un nouveau logo et une charte graphique.

---

Les changelogs précédents et traduits sont archivés dans [`changelogs/`](changelogs/README.md). English version: [`CHANGELOG.md`](CHANGELOG.md).

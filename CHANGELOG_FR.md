# Version 0.1.0 (première version — la réécriture en Rust)

---

## 🦀 Une réécriture de Cadence en Rust

Wavyzon est une réécriture complète de [Cadence](https://github.com/relateful/Cadence), le nœud audio en Go, en **Rust**. Il garde ce qui faisait la force de Cadence — le **mixage à deux platines** avec de vrais cross-fades, l'**horloge de transmission précise à la trame** derrière `TrackAudibleEvent` et les paroles synchronisées, et le **chiffrement de bout en bout DAVE** pour la voix Discord — et reste un **nœud Lavalink v4 compatible** : pointez votre client existant dessus et la musique démarre.

---

## 🔐 DAVE sans libdave

Le chiffrement de la voix, DAVE compris, est désormais **100 % Rust**. Plus aucune bibliothèque native à télécharger ou à lier, et libopus est compilée et liée statiquement : on obtient **un seul binaire autonome**.

---

## 🌐 Une nouvelle source audio HTTP

Lisez des **URL audio directes** et des **flux radio Icecast / Shoutcast** sans rien installer. Les adresses de réseaux privés et locaux sont refusées par défaut (`WAVYZON_HTTP_ALLOW_PRIVATE=false`), pour qu'un nœud ouvert à des clients ne puisse pas être retourné contre votre propre réseau.

---

## 🧩 Une API de plugins

La nouvelle crate `wavyzon-plugin` permet d'ajouter des **sources, des filtres audio, des écouteurs d'événements et des routes REST** sans forker Wavyzon. Les plugins s'activent avec `WAVYZON_PLUGINS` et se configurent avec des variables `WAVYZON_PLUGIN_<NOM>_<CLÉ>`. Un plugin d'exemple est fourni dans `plugins/`.

---

## 🗣️ Synthèse vocale Flowery

Le nouveau plugin `flowery`, **activé par défaut**, fait parler le bot avec la synthèse vocale gratuite de [Flowery](https://flowery.pw). Il lit les identifiants `ftts://<texte>?voice=…&speed=…` de LavaSrc et annonce `ftts` et `flowery-tts` dans `/v4/info` : les recherches `source: "ftts"` de lavalink-client fonctionnent comme sur Lavalink avec LavaSrc. Aucune clé d'API ; `WAVYZON_PLUGIN_FLOWERY_ENABLED=false` le désactive.

---

## 📚 Une meilleure documentation

Un nouveau dossier `docs/` couvre la configuration, l'API, l'API de plugins et le déploiement, et la référence de l'API est publiée depuis `cargo doc` à chaque version.

---

## 🔌 API

- L'API `/v1`, la couche Lavalink v4 et les événements WebSocket sont ceux de Cadence. Nouveau : `GET /v1/plugins`, les routes des plugins sous `/v1/plugins/<nom>`, `plugins` et `pluginFilters` dans `/v1/info`, et `track.identifier` dans les mises à jour de lecteur.
- Les événements supplémentaires en v4 s'activent avec `wavyzonEvents` (le `cadenceEvents` de Cadence fonctionne toujours).
- Les journaux sont en JSON par défaut ; `WAVYZON_LOG_FORMAT=pretty` pour un terminal, et `RUST_LOG` pour filtrer par crate.
- `livecheck` vérifie la recherche, le miroir, la lecture et les sauts contre les sources réelles que vous avez configurées.

---

## 🎵 Sources

- **SoundCloud** est la source de lecture par défaut (`WAVYZON_PLAYBACK_SOURCES`) ; les liens **HTTP** et les fichiers **locaux** se lisent directement.
- **Deezer, Spotify et Apple Music** servent uniquement aux **métadonnées et à la recherche** ; les titres trouvés sont lus depuis une source de lecture. Les options de streaming Deezer (`DEEZER_ARL`, `DEEZER_MASTER_KEY`, ...) disparaissent.

---

## 🐳 Déploiement

- Une nouvelle **image Docker** multi-étapes qui tourne avec un utilisateur non privilégié, et un `docker-compose.yml` pour démarrer.
- Tous les réglages de Cadence passent de `CADENCE_*` à `WAVYZON_*`. Voir `.env.example`.

---

## ⚖️ Licence

Wavyzon est publié sous **CC BY-NC-SA 4.0**, comme les autres projets d'iHorizon. La notice MIT de Cadence est conservée dans `NOTICE`.

---

Les changelogs précédents et traduits sont archivés dans [`changelogs/`](changelogs/README.md). English version: [`CHANGELOG.md`](CHANGELOG.md).

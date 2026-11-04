# Introduction

**Wavyzon** est un nœud audio autonome pour bots Discord, écrit en Rust. Il
rejoint les salons vocaux, diffuse et mixe de la musique, et indique à votre
bot *exactement* quand chaque son a atteint Discord.

Si vous avez déjà utilisé [Lavalink](https://github.com/lavalink-devs/Lavalink),
le modèle est le même : **votre bot réfléchit, Wavyzon s'occupe de l'audio**,
et les deux communiquent par une API REST et un WebSocket. Wavyzon parle même
le protocole v4 de Lavalink : la plupart des bots existants peuvent basculer
sans modifier leur code.

Wavyzon est une réécriture en Rust de [Cadence](https://github.com/relateful/Cadence),
le nœud audio en Go derrière *greed*. Il en reprend le protocole et les deux
grandes idées, l'[horloge de transmission](../wire-clock.md) et le
[mixage à deux platines](../mixing.md), et ajoute :

- une **API de plugins** ([`wavyzon-plugin`](plugins.md)) pour ajouter des
  sources, des filtres, des écouteurs d'événements et des routes HTTP sans
  forker le nœud ;
- une **source audio HTTP** : liens directs vers des fichiers
  MP3/AAC/M4A/FLAC/Ogg/WAV et radios Icecast/Shoutcast, avec sauts par
  requêtes `Range` ;
- des **fichiers locaux**, en option ;
- une pile média **100 % Rust** : [Symphonia](https://github.com/pdeljanov/Symphonia)
  pour le décodage, libopus compilée statiquement, et le chiffrement de bout
  en bout DAVE via la crate [`davey`](https://crates.io/crates/davey). Pas de
  `libdave`, pas de cgo, pas de JVM : un seul binaire statique ;
- cette documentation.

## Vue d'ensemble

```text
 ┌───────────────┐  REST : charger des pistes, piloter    ┌──────────────┐   UDP (Opus, AEAD, DAVE)   ┌─────────┐
 │   votre bot   │ ─────────────────────────────────────► │   Wavyzon    │ ─────────────────────────► │ Discord │
 │ (toute lib.)  │ ◄───────────────────────────────────── │  :2333       │ ◄── passerelle vocale ───► │  voix   │
 └───────┬───────┘  WebSocket : ready, playerUpdate,      └──────┬───────┘                            └─────────┘
         │                     événements                        │
         │ passerelle (VOICE_STATE / VOICE_SERVER_UPDATE)        │ HTTPS
         ▼                                                       ▼
   passerelle Discord                         SoundCloud · HTTP/radio · métadonnées Deezer/Spotify/Apple · plugins
```

1. Votre bot ouvre un WebSocket vers Wavyzon et reçoit un `sessionId`.
2. Il résout ce qu'il veut jouer avec `GET /v1/loadtracks`.
3. Quand Discord envoie à votre bot le serveur vocal et l'état vocal d'une
   guilde, le bot les transmet à Wavyzon dans une mise à jour du lecteur, avec
   la piste.
4. Wavyzon se connecte à la voix Discord, diffuse, et signale les événements
   (`TrackStartEvent`, `TrackAudibleEvent`, `TrackEndEvent`…) sur le WebSocket.

## Par où continuer

| Je veux… | Lire |
| --- | --- |
| Lancer un nœud | [Démarrage rapide](quick-start.md) |
| Brancher un bot Lavalink existant | [Compatibilité Lavalink v4](../lavalink.md) (en anglais) |
| Écrire un client pour le protocole natif | [API REST](../api/rest.md) et [WebSocket](../api/websocket.md) (en anglais) |
| Afficher des paroles synchronisées | [L'horloge de transmission](../wire-clock.md) (en anglais) |
| Ajouter ma propre source ou mon effet | [Écrire des plugins](plugins.md) |

Le reste de la documentation est en anglais pour l'instant ; les
contributions de traduction sont les bienvenues.

## Statut de cette documentation

Wavyzon est jeune. Le protocole, hérité de Cadence, est stable ; certains noms
propres à Wavyzon (quelques variables d'environnement, les noms de
métriques) peuvent encore changer avant la 1.0. Ils sont signalés par :

> Note: provisional

## Licence

Wavyzon est © 2026 Anaïs Saraiva et l'équipe iHorizon, distribué sous licence
[CC BY-NC-SA 4.0](https://creativecommons.org/licenses/by-nc-sa/4.0/deed.fr).
C'est une œuvre dérivée de Cadence (MIT, © 2026 greed) ; voir `NOTICE` dans le
dépôt.

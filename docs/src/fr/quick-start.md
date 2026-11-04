# Démarrage rapide

## 1. Choisir un mot de passe

```bash
export WAVYZON_PASSWORD="$(openssl rand -hex 24)"
```

Wavyzon refuse de démarrer sans `WAVYZON_PASSWORD`, et refuse la valeur par
défaut bien connue de Lavalink, `youshallnotpass`.

## 2. Lancer le nœud

### Avec Docker (recommandé)

```bash
git clone https://gitlab.com/ihrz/wavyzon.git
cd wavyzon
cp .env.example .env        # modifiez au moins WAVYZON_PASSWORD
docker compose up -d        # écoute sur 127.0.0.1:2333
```

L'image contient tout ce qu'il faut pour la voix (Opus est lié statiquement,
DAVE est en Rust pur).

### Depuis les sources

Prérequis : Rust **1.85+** (sélectionné par `rust-toolchain.toml`), un
compilateur C et **CMake** (pour compiler libopus statiquement). Pas de
`libdave`, pas de paquet `libopus`, pas de `pkg-config` à configurer.

```bash
git clone https://gitlab.com/ihrz/wavyzon.git
cd wavyzon
cargo build --release -p wavyzon
WAVYZON_PASSWORD=changez-moi ./target/release/wavyzon
```

Avec le plugin d'exemple :

```bash
cargo build --release -p wavyzon --features plugin-example
WAVYZON_PASSWORD=changez-moi WAVYZON_PLUGINS=example ./target/release/wavyzon
```

> Note: provisional. Le nom exact de la feature (`plugin-<nom>`) est défini
> par la crate binaire `wavyzon`.

## 3. Vérifier

```bash
curl -s -H "Authorization: $WAVYZON_PASSWORD" http://localhost:2333/v1/info
```

```json
{ "version": "0.1.0", "sources": ["deezer", "spotify", "applemusic", "soundcloud", "http"], "dave": true, "mixing": true, "wireClock": true, "…": "…" }
```

Chercher une piste :

```bash
curl -s -H "Authorization: $WAVYZON_PASSWORD" \
  "http://localhost:2333/v1/loadtracks?identifier=scsearch:midnight%20city" | jq '.data[0].info'
```

## 4. Brancher votre bot

### Avec une bibliothèque Lavalink (le plus simple)

Wavyzon est compatible Lavalink v4. Avec
[`lavalink-client`](https://github.com/Tomato6966/lavalink-client) :

```ts
import { LavalinkManager } from "lavalink-client";

client.lavalink = new LavalinkManager({
  nodes: [{ id: "wavyzon", host: "localhost", port: 2333, authorization: process.env.WAVYZON_PASSWORD! }],
  sendToShard: (guildId, payload) => client.guilds.cache.get(guildId)?.shard?.send(payload),
  client: { id: process.env.CLIENT_ID!, username: "mon-bot" },
  playerOptions: { defaultSearchPlatform: "scsearch" }, // pas de YouTube dans Wavyzon
});

client.on("raw", (d) => client.lavalink.sendRawData(d));
client.once("ready", () => client.lavalink.init({ id: client.user!.id, username: client.user!.username }));
```

Remplacez `ytsearch:` par `scsearch:` (ou `spsearch:`, `amsearch:`,
`dzsearch:`, dont les résultats sont mis en miroir vers une source de
lecture). Les autres clients (Shoukaku, Moonlink, Wavelink…) se configurent
de la même façon : hôte, port, mot de passe. Détails :
[Compatibilité Lavalink v4](../lavalink.md).

### Avec le protocole natif `/v1`

1. Ouvrez `GET /v1/websocket` avec les en-têtes `Authorization`, `User-Id`
   (l'identifiant de votre bot) et `Client-Name`. Le premier message est
   `{"op":"ready","sessionId":"…","resumed":false}`.
2. Résolvez une piste avec `GET /v1/loadtracks?identifier=…`.
3. Quand votre bot reçoit `VOICE_STATE_UPDATE` et `VOICE_SERVER_UPDATE` pour
   la guilde, envoyez-les avec la piste :

```http
PATCH /v1/sessions/{sessionId}/players/{guildId}
Authorization: <mot de passe>
Content-Type: application/json

{
  "track": { "encoded": "eyJzb3VyY2UiOiJzb3VuZGNsb3VkIi…" },
  "voice": { "token": "…", "endpoint": "c-rotterdam09.discord.media:443", "sessionId": "…", "channelId": "…" }
}
```

4. Écoutez les événements sur le WebSocket : `TrackStartEvent`,
   `TrackAudibleEvent` (le premier paquet est parti : c'est là qu'on ancre
   les paroles), `TrackEndEvent` avec `reason: "finished"` pour passer à la
   suivante.

Référence complète : [API REST](../api/rest.md) et
[WebSocket et événements](../api/websocket.md).

## 5. Configurer

Les variables principales :

| Variable | Défaut | Description |
| --- | --- | --- |
| `WAVYZON_PASSWORD` | **obligatoire** | Secret partagé |
| `WAVYZON_ADDRESS` | `0.0.0.0:2333` | Adresse d'écoute |
| `WAVYZON_PLAYBACK_SOURCES` | `soundcloud` | Sources vers lesquelles les pistes Spotify/Apple/Deezer sont mises en miroir |
| `WAVYZON_HTTP_ALLOW_PRIVATE` | `false` | Laisser la source HTTP joindre des adresses privées. **Laissez à `false`** si des clients non fiables peuvent atteindre le nœud |
| `WAVYZON_LOCAL_ROOT` | désactivé | Dossier lisible par la source de fichiers locaux |
| `WAVYZON_PLUGINS` | tous ceux compilés | Plugins à activer (vide = tous les plugins compilés dans le binaire) |
| `SPOTIFY_CLIENT_ID` / `SPOTIFY_CLIENT_SECRET` | | Pour les liens Spotify |

Toutes les variables : [Configuration](../configuration.md).

## Et ensuite ?

- Mettre le nœud derrière TLS et le surveiller : [Déploiement](../deployment.md),
  [Observabilité](../observability.md).
- Écrire votre propre source ou filtre : [Écrire des plugins](plugins.md).
- Ça ne marche pas ? [Dépannage](../troubleshooting.md).

# Écrire des plugins

On peut étendre Wavyzon sans le forker. Un plugin est une **crate Rust**
ordinaire qui dépend de [`wavyzon-plugin`](https://gitlab.com/ihrz/wavyzon/-/tree/main/crates/wavyzon-plugin)
et implémente un trait, `Plugin`. Il peut ajouter :

| Extension | Trait(s) | Visible comme |
| --- | --- | --- |
| [Sources](#2-sources) | `Source` (+ `Playable`, `IsrcLookup`, `Recommender`) | préfixes et liens de `loadtracks`, lecture, cibles du miroir ISRC |
| [Filtres audio](#3-filtres-audio) | `FilterFactory` → `AudioFilter` | `filters.pluginFilters.<nom>` |
| [Écouteurs d'événements](#4-écouteurs-dévénements) | `EventListener` | chaque événement de chaque lecteur |
| [Routes HTTP](#5-routes-http) | un `axum::Router` | `/v1/plugins/<nom>/…` |

L'exemple complet et testé utilisé sur cette page se trouve dans
[`plugins/wavyzon-plugin-example`](https://gitlab.com/ihrz/wavyzon/-/tree/main/plugins/wavyzon-plugin-example).
La [version anglaise](../plugins.md) de ce guide contient le code intégral de
chaque exemple.

## Chargement des plugins

Les plugins sont **compilés dans le binaire**, pas chargés dynamiquement :

1. une **feature cargo** du binaire `wavyzon` lie la crate du plugin ;
2. au démarrage, le nœud instancie les plugins compilés nommés dans
   `WAVYZON_PLUGINS`, dans cet ordre (tous les plugins compilés si la
   variable est vide) ;
3. il lit le `PluginManifest` et refuse un plugin compilé pour une autre
   version majeure de l'API ([versions](#versions)) ;
4. il construit un `PluginContext` (réglages tirés de
   `WAVYZON_PLUGIN_<NOM>_*`, dossier de données privé) et appelle
   `register`, où le plugin ajoute ses extensions au `Registry` ;
5. seulement ensuite, le serveur commence à écouter. À l'arrêt propre, la
   méthode `shutdown` de chaque plugin est appelée.

Pourquoi à la compilation ? Rust n'a pas d'ABI stable : des plugins
dynamiques devraient de toute façon correspondre exactement au compilateur et
aux dépendances du nœud, avec du FFI `unsafe` en prime. Compilés, ils sont
vérifiés par le typage, tournent à pleine vitesse sur le chemin audio temps
réel, et on garde un binaire statique unique.

## 1. Créer la crate

```toml
# Cargo.toml
[package]
name = "wavyzon-plugin-bonjour"
version = "0.1.0"
edition = "2024"

[dependencies]
wavyzon-plugin = { git = "https://gitlab.com/ihrz/wavyzon", tag = "v0.1.0" }
tokio = { version = "1", features = ["io-util"] }   # seulement si vous implémentez Playable
tracing = "0.1"                                        # facultatif
```

Dans l'espace de travail Wavyzon, utilisez l'héritage :
`wavyzon-plugin.workspace = true`, `version.workspace = true`, etc.

`wavyzon-plugin` ré-exporte `async_trait`, `axum` et `serde_json` :
**utilisez ces ré-exports** pour que vos routes et vos configurations de
filtres soient exactement les types attendus par le nœud.

### Le plus petit plugin possible

```rust
use std::sync::Arc;
use wavyzon_plugin::*;

pub struct Bonjour;

#[async_trait]
impl Plugin for Bonjour {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            name: "bonjour",
            version: env!("CARGO_PKG_VERSION"),
            description: "Dit bonjour",
            api_version: API_VERSION,
        }
    }

    async fn register(&self, ctx: &PluginContext, reg: &mut Registry) -> Result<(), PluginError> {
        let salut = ctx.setting("salutation").unwrap_or("bonjour").to_string();
        reg.routes(axum::Router::new().route("/", axum::routing::get(move || async move { salut })));
        Ok(())
    }
}

/// Point d'entrée utilisé par le nœud.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(Bonjour)
}
```

```bash
WAVYZON_PLUGINS=bonjour WAVYZON_PLUGIN_BONJOUR_SALUTATION=salut ./wavyzon
curl -H "Authorization: $WAVYZON_PASSWORD" localhost:2333/v1/plugins/bonjour/
# salut
```

### Le manifeste

| Champ | Règles |
| --- | --- |
| `name` | Minuscules, sans caractère spécial d'URL, unique. C'est le préfixe de route `/v1/plugins/<nom>`, l'entrée de `WAVYZON_PLUGINS` et le préfixe d'environnement `WAVYZON_PLUGIN_<NOM>_` (en majuscules, `-` → `_`) |
| `version` | Version de votre plugin : `env!("CARGO_PKG_VERSION")` |
| `description` | Une ligne, affichée dans `/v1/plugins` et `/v1/info` |
| `api_version` | Toujours `API_VERSION` |

### Réglages et données

- `ctx.settings` contient toutes les variables `WAVYZON_PLUGIN_<NOM>_*`, avec
  pour clé le suffixe **en minuscules** ; `ctx.setting(clé)` met la clé en
  minuscules pour vous.
- Renvoyez `PluginError::Config` pour un réglage invalide : le nœud refuse de
  démarrer et affiche votre message.
- `ctx.data_dir` (`$WAVYZON_DATA_DIR/plugins/<nom>`) est un dossier privé
  inscriptible, **non créé** automatiquement.

## 2. Sources

Une source transforme requêtes et liens en `Track`. Une source **jouable**
(`playable()` renvoie `Some`) sait aussi ouvrir un flux audio.

| Le client envoie | Le nœud appelle |
| --- | --- |
| `loadtracks?identifier=<préfixe>:<requête>` | `search(requête)` |
| `loadtracks?identifier=<url>` où `matches_url(url)` | `load_url(url)` ; les sources de plugins passent **avant** les sources intégrées |
| Une mise à jour de lecteur avec une de vos pistes | `playable().open(piste, start_ms)` |
| Un miroir Spotify/Apple/Deezer, si votre source est dans `WAVYZON_PLAYBACK_SOURCES` | `isrc_lookup().by_isrc(isrc)` |

Points importants :

- L'`identifier` d'une piste doit se suffire à lui-même : la piste est
  encodée, envoyée au client, et peut revenir **sur un autre nœud ou après un
  redémarrage**.
- Erreurs : `Failure::common` (attendu), `Failure::suspicious` (refus de la
  source : 401/403/429), `Failure::fault` (bug), ou
  `Failure::from_status(source, statut, cause)`.
- Formats de flux : `Format::Encoded { hint }` pour les octets d'un fichier
  MP3, AAC/M4A, FLAC, Ogg Vorbis ou WAV (décodés par Symphonia), ou
  `Format::Pcm48` pour du PCM brut **16 bits signé little-endian, stéréo
  entrelacé, 48 kHz** (4 octets par échantillon stéréo).
- Sauts : si vous pouvez commencer à `start_ms` à moindre coût, faites-le,
  mettez `start_sample = start_ms * 48` et renvoyez `true` dans `seekable()`.
  Sinon laissez `start_sample = 0` : le lecteur décode depuis le début et
  avance tout seul.

Extrait de la source `tone` du plugin d'exemple (`tone:440`, `tone:440:30`,
`tone:440:live`) :

```rust
#[async_trait]
impl Source for ToneSource {
    fn name(&self) -> &str { "tone" }
    fn search_prefix(&self) -> Option<&str> { Some("tone") }
    async fn search(&self, query: &str) -> Result<Vec<Track>, Failure> {
        Ok(vec![ToneSpec::parse(query, &self.settings)?.to_track()])
    }
    fn matches_url(&self, url: &str) -> bool { url.starts_with("tone://") }
    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        let rest = url.strip_prefix("tone://").unwrap_or(url);
        Ok(LoadResult::single(ToneSpec::parse(rest, &self.settings)?.to_track()))
    }
    async fn track(&self, identifier: &str) -> Result<Track, Failure> {
        Ok(ToneSpec::parse(identifier, &self.settings)?.to_track())
    }
    fn playable(&self) -> Option<&dyn Playable> { Some(self) }
}

#[async_trait]
impl Playable for ToneSource {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure> {
        let spec = ToneSpec::parse(&track.identifier, &self.settings)?;
        let start = start_ms.max(0) * SAMPLE_RATE as i64 / 1000;
        Ok(Stream {
            body: Box::new(ToneReader::new(spec, self.settings.amplitude, start)),
            format: Format::Pcm48,
            start_sample: start,           // le saut est respecté
            live: spec.seconds.is_none(),
        })
    }
    fn seekable(&self) -> bool { true }
}
```

## 3. Filtres audio

- La fabrique (`FilterFactory`) est partagée ; `create` construit **une
  instance par platine** de chaque lecteur (deux pendant un fondu), et sert
  aussi à valider une nouvelle configuration. Une erreur est renvoyée au
  client en `400`.
- `process` reçoit une trame de 20 ms : `FRAME_SAMPLES` (1 920) échantillons
  `f32` stéréo entrelacés à `SAMPLE_RATE` (48 000), dans `-1.0..=1.0`, à
  modifier sur place.
- `process` tourne sur le **chemin temps réel** : pas de blocage, pas d'E/S,
  pas d'allocation par trame.
- `reconfigure` applique une nouvelle configuration à chaud et renvoie
  `true` ; `false` fait reconstruire le filtre (avec un fondu entre
  l'ancien et le nouveau).
- `reset` est appelé lors d'un saut et d'un changement de piste.
- Les filtres ajoutés ou retirés apparaissent et disparaissent en fondu : le
  nœud s'en charge.

Extrait du filtre `bitcrush` :

```rust
impl AudioFilter for Bitcrush {
    fn process(&mut self, frame: &mut [f32]) {
        let (wet, dry) = (self.cfg.mix, 1.0 - self.cfg.mix);
        for pair in frame.chunks_exact_mut(CHANNELS) {
            if self.countdown == 0 {
                for (ch, s) in pair.iter().enumerate() {
                    self.held[ch] = ((*s * self.steps).round() / self.steps).clamp(-1.0, 1.0);
                }
                self.countdown = self.cfg.downsample;
            }
            self.countdown -= 1;
            for (ch, s) in pair.iter_mut().enumerate() {
                *s = *s * dry + self.held[ch] * wet;
            }
        }
    }
    fn reset(&mut self) { self.held = [0.0; CHANNELS]; self.countdown = 0; }
}

impl FilterFactory for BitcrushFactory {
    fn name(&self) -> &str { "bitcrush" }
    fn create(&self, config: &Value) -> Result<Box<dyn AudioFilter>, Failure> {
        Ok(Box::new(Bitcrush::new(BitcrushConfig::parse(config)?)))
    }
}
```

Côté client :

```json
{ "filters": { "pluginFilters": { "bitcrush": { "bits": 6, "downsample": 4 } } } }
```

## 4. Écouteurs d'événements

`EventListener::on_event(session_id, event)` est appelé pour **chaque**
événement de chaque lecteur, après son envoi au client, sur une tâche
dédiée : un écouteur lent ne ralentit que lui-même. Le nom « fil » d'un
`EventType` est celui de sa sérialisation serde (`EventType::PositionJump`
s'appelle `"PositionEvent"`). Pensez à un bras `_ =>` dans vos `match` : de
nouveaux types d'événements peuvent apparaître.

```rust
#[async_trait]
impl EventListener for EventCounter {
    async fn on_event(&self, session_id: &str, event: &Event) {
        *self.by_type.lock().unwrap().entry(Self::wire_name(event.kind)).or_default() += 1;
        self.total.fetch_add(1, Ordering::Relaxed);
        if let (EventType::TrackExceptionEvent, Some(f)) = (event.kind, &event.exception) {
            tracing::warn!(session_id, guild = %event.guild_id, "échec de piste : {f}");
        }
    }
}
```

## 5. Routes HTTP

`reg.routes(router)` monte un `Router` [axum 0.8](https://docs.rs/axum/0.8)
sur `/v1/plugins/<nom>`. Les requêtes sont **authentifiées par le nœud**
avant de vous parvenir. Passez l'état avec `.with_state(...)` (le routeur
doit être un `Router<()>`), et n'appelez `routes` qu'une fois par plugin.

```rust
pub fn routes(shared: Arc<Shared>) -> Router {
    Router::new().route("/", get(stats)).route("/reset", post(reset)).with_state(shared)
}
```

```bash
curl -H "Authorization: $WAVYZON_PASSWORD" localhost:2333/v1/plugins/example/
```

## Activer un plugin dans le nœud

```toml
# crates/wavyzon/Cargo.toml
[dependencies]
wavyzon-plugin-example = { path = "../../plugins/wavyzon-plugin-example", optional = true }

[features]
plugin-example = ["dep:wavyzon-plugin-example"]
```

```bash
cargo build --release -p wavyzon --features plugin-example
WAVYZON_PLUGINS=example WAVYZON_PLUGIN_EXAMPLE_AMPLITUDE=0.3 ./target/release/wavyzon
```

> Note: provisional. Les noms de features, la convention de point d'entrée
> `plugin()` et le fichier qui liste les plugins compilés dépendent du
> binaire `wavyzon` ; le trait `Plugin` lui-même est stable.

## Tester

Tout se teste sans nœud : `cargo test -p wavyzon-plugin-example`. Appelez
`register` sur un `Registry::new()` (après `reg.begin("<nom>")`, comme le
fait le nœud), lisez les flux de vos sources avec `AsyncReadExt`, passez des
trames de `FRAME_SAMPLES` à vos filtres, et appelez directement vos
gestionnaires de routes.

## Versions

`wavyzon_plugin::API_VERSION` vaut actuellement **`1`**.

- Il augmente quand l'API change d'une façon qui casse les plugins existants.
- Le nœud **refuse** au démarrage un plugin dont `api_version` diffère du
  sien (`PluginError::ApiVersion`).
- Les ajouts compatibles (nouvelle méthode avec implémentation par défaut,
  nouveau type d'événement) ne le changent pas.

| Wavyzon | `API_VERSION` |
| --- | --- |
| 0.1.x | 1 |

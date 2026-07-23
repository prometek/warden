# Code Standards

> Ce fichier liste les conventions du projet Warden. Toute exception doit être justifiée en commentaire dans le code concerné. Stack réelle : Rust (tokio, sqlx, ratatui), SQLite, workspace multi-binaires (`warden`, `warden-gated`, `warden-tui`).

## General

- Garder les modules petits et à responsabilité unique.
- Pas de logique métier dans les handlers, commandes CLI, ou callbacks d'event — déléguer à une couche core.
- Séparer strictement I/O (process, DB, git, réseau) et logique pure (state machine, interprétation des findings) : la logique pure doit être testable sans filesystem, sans DB, sans subprocess.
- Composition plutôt qu'héritage. Pas de god object.
- Corriger la cause racine d'un bug, jamais empiler des workarounds.
- Valider toute entrée externe (stdout d'agent, ligne de commande, event, ligne SQLite) à la frontière, avant qu'elle n'atteigne la state machine.
- Ne jamais faire confiance à la sortie d'un agent CLI — elle est suspecte jusqu'à parsing validé.
- Erreurs typées explicitement, jamais des strings ou codes magiques.
- Pas de magic numbers : constantes nommées.
- Toute logique non triviale a des tests (happy path + cas d'erreur principal).
- Tests déterministes : pas de temps réel non mocké, pas d'appel réseau externe, pas de dépendance à l'ordre.
- Commentaires uniquement pour le *pourquoi* non-évident.
- Nommage explicite : pas de `tmp`, `data2`, `foo`. Fonctions < ~50 lignes.
- Pas de secret en clair dans le code ou le repo (voir `## Secrets Management`).
- Pas de logs contenant PII ou secrets.
- **Anti-patterns proscrits** : trust agent output, god object, spaghetti, fonctions trop longues, workarounds successifs, catch-and-ignore (`.ok()` qui jette l'erreur sans la logger).

## Rust

### Structure

- Workspace Cargo : un crate `[lib]` pour la logique core, un `[[bin]]` par exécutable (`warden`, `warden-gated`, `warden-tui`).
- Chaque `main.rs` minimal : parsing CLI + dispatch vers la lib. Aucune logique métier.
- La logique core (state machine, convergence, interprétation des findings) vit dans la lib, 100% testable sans I/O.
- `error.rs` par crate pour les types d'erreurs custom + `Result` local.
- Séparation stricte I/O vs logique pure.

### Erreurs

- `thiserror::Error` pour tous les types d'erreur de lib.
- `anyhow::Result` réservé aux `main` des binaires.
- Exporter un `type Result<T>` local depuis `error.rs`. `#[from]` pour les conversions std.
- Aucun `unwrap()` ni `expect()` hors tests. Pas de `panic!` en code applicatif sauf invariant strict documenté.

### CLI

- `clap` feature `derive`. `#[command(version, about)]` toujours présent.
- Subcommands pour les modes distincts (`warden run`, `warden tui`, `warden-gated serve`).
- Chaque argument documenté via `///`. `value_parser` pour valider au niveau clap.

### Async & concurrence

- `tokio` runtime unique par binaire. Pas de `block_on` imbriqué.
- Parallélisme review/test : chaque agent d'un cycle tourne dans sa propre task, sur son propre worktree (aucun état mutable partagé entre tasks concurrentes).
- Pas de parallélisme prématuré. `rayon` uniquement si CPU-bound sur volume significatif (pas le cas ici : tout est I/O-bound subprocess).

### Logging

- `tracing` + `tracing-subscriber`. Initialisation dans chaque `main`, jamais dans la lib.
- Niveau contrôlé via `--verbose` (`-v`/`-vv`/`-vvv`).
- La lib émet des spans/events `tracing` ; elle n'écrit jamais sur stdout/stderr directement (réservé à la sortie structurée des binaires).

### Dépendances

- N'inclure que les dépendances réellement utilisées. Préférer la stdlib.
- Pinning a minima sur la majeure dans `Cargo.toml`. `Cargo.lock` committé (binaires).

### Idiomes

- Itérateurs plutôt que boucles `for` quand plus clair. `?` pour la propagation, pas de `match` verbeux inutile.
- Pas de `clone()` réflexe — chaque clone justifié.

### Tests

- Tests unitaires inline sous `#[cfg(test)] mod tests` dans la lib.
- Tests d'intégration dans `tests/`. `assert_cmd` + `predicates` pour tester les binaires compilés.
- `tempfile::TempDir` pour les tests filesystem/git.
- DB de test : SQLite fichier temporaire réel via `tempfile`, jamais de mock (voir `## SQLite & sqlx`).
- Tester systématiquement : happy path, sortie JSON, cas d'erreur principal.

## SQLite & sqlx

*(Section ad-hoc — conventions dérivées des ADR-0004/0006.)*

- SQLite unique fichier sous `~/.warden/` comme état persistant des runs, cycles, findings et **events**.
- Accès via `sqlx` avec requêtes vérifiées à la compilation (`query!`/`query_as!`) — pas de SQL construit par concaténation de strings.
- **State machine explicite `RunState`** : toute transition est écrite en base **avant** de déclencher l'action correspondante (write-ahead d'intention). Jamais l'inverse.
- `warden` est le seul writer. `warden-gated` et `warden-tui` ouvrent la base en **lecture seule** et revérifient l'état de manière indépendante avant d'agir.
- Migrations versionnées via `sqlx::migrate!` (dossier `migrations/`), appliquées au démarrage de `warden`. Migrations atomiques, jamais éditées après commit — toute correction = nouvelle migration.
- `WAL` activé pour permettre la lecture concurrente (TUI/gated) pendant les writes de `warden`.
- Validation à la frontière : toute ligne relue est reparsée en type Rust fort, jamais consommée en tuple brut au-delà de la couche d'accès.

## Agent Subprocess Protocol

*(Section ad-hoc — conventions dérivées de l'ADR sur l'agnosticisme d'agent.)*

- Agents CLI (coder, reviewer, tester, doc) invoqués en sous-processus via `tokio::process::Command`. Jamais d'appel direct à une API LLM depuis Warden (préserve l'agnosticisme d'agent, Warden ne détient aucune clé API d'agent).
- `cwd` du subprocess pointé sur le worktree isolé du run/rôle : `~/.warden/worktrees/<run_id>/<role>`.
- Échange JSON en streaming sur stdin/stdout. Chaque ligne stdout est une valeur JSON validée (parse + schéma) avant d'atteindre la state machine ; ligne non parsable = finding d'erreur, pas un panic.
- stderr de l'agent capturé et loggé via `tracing`, jamais interprété comme donnée de contrôle.
- Timeout par invocation d'agent : un agent qui ne termine pas dans le budget imparti est tué et traité comme finding bloquant.
- Le worktree est créé via `git worktree add --detach` par run/rôle, nettoyé en fin de run.

## TUI (ratatui)

*(Section ad-hoc — conventions dérivées de l'exigence "observation en direct, sans capacité d'action".)*

- `warden-tui` est un **process séparé, strictement en lecture seule** : aucune capacité d'action sur le pipeline. Ne write jamais la base, ne spawn jamais d'agent, ne touche jamais git.
- Direct : abonnement à l'Event Bus. Historique/replay au moment de l'attache : lecture de la table `events` en SQLite (read-only).
- Rendu ratatui découplé de la source de données : la couche modèle (state du run) est alimentée par le bus + la DB, le rendu ne fait que projeter ce modèle. Testable sans terminal.
- Aucune logique métier dans le code de rendu — pas d'interprétation de findings côté TUI.
- Attachable depuis le terminal lanceur **ou** un terminal séparé, sans effet de bord sur le run.

## Inter-process Communication

*(Le projet a 3 binaires qui communiquent — conventions de frontière.)*

- **`warden` → `warden-gated`** : via un **remote git bare local** + hook `post-receive`. Warden push le résultat converged vers le bare remote ; le hook notifie `warden-gated`. Aucun autre canal de commande entre les deux.
- **`warden-gated` ne fait jamais confiance à la notification seule** : il revérifie systématiquement l'état du run en SQLite (state == `Converged`, hash committé == attendu) avant toute action vers le remote réel ou le fournisseur PR/CI.
- **`warden` → `warden-tui`** : Event Bus (direct) + table `events` SQLite (replay). Unidirectionnel — la TUI n'émet jamais vers Warden.
- Push vers `origin` réel : **uniquement** par `warden-gated`, **uniquement** si l'état persisté est `Converged` avec vérification du hash. `warden` et `warden-tui` n'ont aucun accès aux credentials du remote réel.
- Tout message franchissant une frontière (event, ligne DB, notification hook) est validé/reparsé côté récepteur avant usage.

## File Organization

### Workspace Rust

- `Cargo.toml` racine — définition du `[workspace]` et des membres.
- `crates/warden-core/` — `[lib]`, logique core (state machine, convergence, types de findings). 100% testable sans I/O.
- `crates/warden/` — `[[bin]]` orchestrateur (tokio, spawn agents, writer SQLite).
- `crates/warden-gated/` — `[[bin]]` gate (credentials, post-receive, PR/CI).
- `crates/warden-tui/` — `[[bin]]` TUI ratatui (read-only).
- Chaque crate : `src/main.rs` minimal (bins), `src/lib.rs`, `src/error.rs`, `src/{domain}.rs` par domaine cohérent.
- `migrations/` — migrations sqlx SQLite (au niveau du crate `warden` ou partagé).
- `tests/` — tests d'intégration par crate (binaire compilé via `assert_cmd`).
- Tests unitaires inline sous `#[cfg(test)] mod tests`.
- `Cargo.lock` committé.

## Git & CI

- **Conventional Commits** : `type(scope?): description`. Types : `feat`, `fix`, `chore`, `refactor`, `docs`, `test`, `perf`, `build`, `ci`, `style`, `revert`. Scope recommandé (`feat(gated):`, `fix(tui):`, `feat(core):`). Impératif présent, minuscule, pas de point final. Breaking : `!` ou footer `BREAKING CHANGE:`.
- Branche principale `main`. Branches de travail `feat/...`, `fix/...`, `chore/...`. Pas de commit direct sur `main` — tout passe par PR titrée en Conventional Commit.
- **Pre-commit** (issue #69) — hooks shell versionnés dans `.githooks/`, activés via `git config core.hooksPath .githooks` (natif git, zéro dépendance externe — pas de framework `pre-commit`). Sous-ensemble de la CI (secondes sur cache chaud, jusqu'à quelques minutes à froid car `clippy` lint tout le workspace) :
  - `cargo fmt --all --check` — bloquant (pas d'auto-fix silencieux : `cargo fmt --all` puis re-stage manuel).
  - `cargo clippy --all-targets --all-features -- -D warnings` — bloquant.
  - Détection de secrets (`gitleaks protect --staged`, si le binaire est installé — sinon avertissement, jamais un blocage silencieux masqué) — bloquant.
  - Exclus (restent en CI uniquement, jamais dans les hooks) : tests, build release.
- `--no-verify` toléré en cas exceptionnel (WIP branche perso, debug CI), jamais sur `main` ni PR prête à merger.
- **GitHub Actions** — déclenché sur PR et push `main`, toutes étapes bloquantes :
  - `cargo clippy -- -D warnings`.
  - `cargo fmt --check`.
  - `cargo check` (workspace).
  - `cargo test` (workspace, unit + intégration).
  - `cargo build --release` des trois binaires.
  - Cache Cargo configuré.
- Pas de merge si une étape échoue.

## Secrets Management

- **SOPS + age** pour tout secret. Aucun `.env` en clair committé, aucun secret en plain text dans la CI.
- Credentials du remote réel et du fournisseur PR/CI : détenus **exclusivement** par `warden-gated` à l'exécution, jamais par `warden` ni `warden-tui`. Fournis via env/keychain au démarrage de `warden-gated`, jamais hardcodés.
- Fichiers chiffrés committés (`.env.enc`, `secrets.enc.yaml`) ; `.sops.yaml` racine liste les destinataires age.
- `.gitignore` exclut `.env` non chiffré, `*.key`, `*.pem`.
- Clé age dédiée CI stockée dans les secrets GitHub Actions. Clé privée dev locale (`~/.config/sops/age/keys.txt`), jamais committée.
- Secret committé par erreur = compromis : rotation immédiate + purge historique (`git filter-repo`).
- Aucun secret en clair dans code, commits, logs, messages d'erreur, PR, issues.

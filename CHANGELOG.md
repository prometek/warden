# Changelog

Toutes les modifications notables de ce projet sont documentées dans ce fichier.

Le format s'appuie sur [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/),
et ce projet suit [Semantic Versioning](https://semver.org/lang/fr/) une fois publié.

## [Unreleased]

### Added — Phase 1 : fondations

- Workspace Cargo avec deux crates : `warden-core` (state machine des runs et
  interprétation des findings, logique pure sans I/O) et `warden` (binaire
  orchestrateur).
- Binaire `warden` avec sous-commande `run` pilotant une boucle de convergence
  séquentielle (coder → review/test → reboucle si findings bloquants) sur un dépôt
  utilisateur, via des agents CLI lancés en sous-processus (`--coder-cmd`,
  `--reviewer-cmd`, `--tester-cmd`).
- Protocole de sortie des agents en NDJSON (`warden_core::parse_findings`) : chaque
  ligne stdout est un finding JSON validé à la frontière (source, sévérité, fichier,
  description, action).
- Gestion des worktrees git isolés par run/rôle (`git worktree add --detach`), jamais
  d'écriture directe dans le dépôt de l'utilisateur.
- Persistance de l'état des runs (SQLite, `sqlx`, migrations versionnées) : runs,
  cycles, findings, suivi du commit SHA du coder.
- Reprise après crash au démarrage (`recover_crashed_runs`) : tout run laissé actif
  sans processus agent vivant est marqué `Failed`.
- Compilation et tests entièrement hors ligne grâce au cache `sqlx` committé
  (`crates/warden/.sqlx/`) — pas de `DATABASE_URL` requis pour `cargo build` /
  `cargo test`.

### Changed — Phase 2 : parallélisme réel

- Reviewer et tester ne tournent plus séquentiellement : `run_review_and_test`
  les lance désormais en parallèle (`tokio::join!`), chacun dans son propre
  worktree synchronisé sur le commit du coder, sans état partagé entre les
  deux. Le temps de cycle est donc borné par le plus lent des deux agents et
  non plus par leur somme ; aucun changement fonctionnel sur les findings
  produits.
- Timeout de verrou SQLite (`busy_timeout`) rendu explicite sur la connexion,
  en anticipation des écritures concurrentes reviewer/tester introduites par
  ce parallélisme.

### Added — Phase 3 : gate git (`warden-gated`)

- Nouveau binaire `warden-gated`, membre du workspace, seul détenteur des
  credentials vers `origin` (ADR-0002/ADR-0006). Ne dépend pas du crate
  `warden` : sa lecture de la base est volontairement dupliquée
  (`warden-gated::db`) pour ne jamais hériter d'un bug de la couche
  d'accès de `warden`.
- Dépôt bare git local + hook `post-receive` minimal (relais uniquement,
  aucune logique métier) : `warden-gated init-bare` le crée et installe le
  hook, `warden-gated notify` (invoqué par le hook) relaie le payload brut
  via un socket Unix vers le daemon `warden-gated serve`.
- Revérification indépendante avant tout push vers `origin` :
  `warden-gated` rouvre la SQLite de `warden` en connexion strictement
  **lecture seule** et ne relaie le push que si `state == Converged` et le
  commit poussé correspond au hash validé (`runs.converged_commit_sha`) —
  jamais de confiance aveugle envers ce que `warden` prétend.
- Sous-commande de diagnostic `warden-gated verify-run` pour rejouer cette
  revérification indépendamment de tout push réel.
- Fichiers de service managé fournis (`crates/warden-gated/contrib/`) :
  unité systemd utilisateur (Linux) et agent launchd (macOS), pour que le
  gate survive à un redémarrage machine.
- Cache `sqlx` offline propre au crate (`crates/warden-gated/.sqlx/`),
  indépendant de celui de `warden`.

### Security — Phase 3 : durcissement post-revue (`warden-gated`)

- Socket Unix du daemon `warden-gated serve` désormais restreint en
  lecture/écriture au seul propriétaire (`0600`) juste après le `bind` — le
  mode par défaut du système n'était pas suffisamment restrictif — aligné sur
  le `0600` déjà exigé par l'ADR-0008 pour le socket équivalent de la TUI.
- Traitement des payloads `post-receive` multi-refs ligne par ligne : une
  ligne malformée, ou dont la revérification/le push échoue, est désormais
  journalisée et ignorée individuellement plutôt que d'annuler tout le lot.
- Documentation corrigée pour ne plus survendre la frontière de sécurité :
  les unités systemd/launchd fournies font tourner `warden-gated` sous le
  **même** utilisateur OS que `warden` (frontière process/logique, pas
  isolation OS) ; le README documente désormais cette limite et la
  configuration à utilisateur OS dédié nécessaire pour une isolation qui
  tient face à un `warden` compromis au niveau code ("Déploiement durci").
- Clarification README : `warden` ne pousse pas encore automatiquement les
  runs convergés vers le dépôt bare du gate (câblage `Converged` → `Pushed`
  prévu en Phase 4) ; le mécanisme du gate lui-même est complet et testé de
  bout en bout indépendamment de ce câblage.

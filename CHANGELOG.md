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

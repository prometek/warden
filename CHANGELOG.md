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

### Added — Phase 4 : PR Manager (`warden-gated`)

- Nouveau module `pr_manager` dans `warden-gated`, exposant trois actions de
  bibliothèque formant le cycle de vie d'une PR (ADR-0007) :
  - `OpenDraft` — pousse un commit squelette de branche **sans aucun contenu
    métier** et ouvre une PR en draft, liée à l'issue détectée dans l'intent
    (`(?i)(fixes|closes|resolves)\s+#\d+`) ou titrée à partir de l'intent
    sinon.
  - `PostCycleUpdate` — poste un commentaire informatif par cycle (findings
    reviewer/tester) sur la PR ; ne modifie jamais son statut draft ni son
    contenu.
  - `Finalize` — revérifie `state == Converged` et le hash committé via le
    même chemin `gate::verify_and_authorize` que le gate git lui-même (jamais
    une vérification séparée et plus faible), puis seulement si autorisé :
    pousse le contenu réel, met à jour le corps de la PR et retire le statut
    draft.
- Vérification indépendante « contenu vide » avant tout push de squelette par
  `OpenDraft` : comparaison d'arbre net contre la branche de base **et**
  parcours commit par commit de l'historique effectivement transféré
  (`git diff-tree --cc`, pour couvrir aussi le contenu propre d'un commit de
  fusion) — un squelette qui changerait le moindre fichier est refusé plutôt
  que poussé.
- Attributs de commit structurés (`Warden-Cycle`, `Warden-Findings-Resolved`,
  `Warden-Agent`) formatés par ce même module, destinés aux commits
  coder/doc.
- Trait `PrProvider`, seam fine au-dessus d'un fournisseur de PR, avec une
  implémentation GitHub (`gh_provider::GhProvider`) qui pilote la CLI `gh`
  déjà authentifiée sur la machine — `warden-gated` ne stocke ni ne lit
  lui-même de credential GitHub.
- Ces trois actions n'existent pour l'instant que comme capacité de
  bibliothèque : aucun déclenchement CLI/IPC ne les invoque encore depuis
  `warden` (câblage laissé à une décision d'architecture distincte, hors
  périmètre de cette livraison).

### Added — Phase 5 : CI Watcher (`warden-gated`)

- Nouveau module `ci_watcher` dans `warden-gated`, qui surveille une PR déjà
  ouverte jusqu'à un statut terminal : `Merged`, `Closed` (fermée sans
  merge), `ChecksPassed`, `ChecksFailed` (avec un finding bloquant par check
  en échec, `FindingSource::Ci`), ou `TimedOut` (timeout d'inactivité
  configurable — jamais d'attente infinie).
- Boucle de polling (`watch_pr`) qui ne fait jamais de busy-spin (`sleep`
  entre deux appels) et réinitialise son horloge d'inactivité à chaque
  changement de statut observé — une CI qui progresse réellement (nouveaux
  checks, check en cours qui se termine) n'est jamais interrompue
  prématurément ; seul un statut resté strictement inchangé pendant la durée
  configurée déclenche le `TimedOut`.
- Trait `CiProvider`, seam au-dessus d'un fournisseur de statut PR/CI,
  implémenté pour GitHub par `gh_provider::GhProvider` (`gh pr view --json
  state,statusCheckRollup`), avec reconnaissance des deux formats de check
  que GitHub peut renvoyer (Checks API récente et Statuses API historique).
- Sous-commande `warden-gated watch-pr` (mêmes conventions que
  `verify-run` : code de sortie `0` pour `Merged`/`ChecksPassed`, non nul
  sinon) — validée de bout en bout sur de vrais dépôts GitHub publics : PR
  mergée, PR ouverte aux checks tous verts, et PR sans aucune CI configurée
  (timeout d'inactivité déclenché proprement, sans busy-spin).
- Fonction pure `warden_core::decide_next_state_after_ci`, miroir de
  `decide_next_state` pour les findings reviewer/tester : décide le
  `RunState` (`Done` / `CoderRunning` / `Failed`) à partir du résultat
  terminal du watcher. `warden-gated` ne fait que remonter le résultat ;
  c'est l'orchestrateur (`warden`) qui déciderait, via cette fonction, de
  reboucler vers le coder — le câblage réel de cette décision dans la
  boucle de convergence reste, comme pour le PR Manager (Phase 4), une
  décision d'architecture distincte hors périmètre de cette livraison.
- **Aucun merge automatique** : `CiProvider`/`ci_watcher` n'exposent aucune
  capacité de merge, quel que soit le statut observé — la décision de merger
  reste entièrement humaine, y compris une fois `ChecksPassed` atteint.

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

### Added — Phase 6 : résilience et récupération après crash

- `recover_crashed_runs` va désormais au-delà du simple marquage `Failed` :
  au démarrage de `warden`, les runs laissés dans un état intermédiaire
  (`CoderRunning`, `AwaitingReviewTest`, `AwaitingCi`) sans processus agent
  vivant associé sont marqués `Failed`, puis leurs ressources orphelines
  sont récupérées automatiquement — processus agents encore vivants
  terminés (identification par PID *et* heure de démarrage enregistrée,
  robuste à une réutilisation de PID par l'OS) et worktrees git orphelins
  supprimés (`git worktree remove --force` + `git worktree prune`), sans
  aucune intervention manuelle.
- Cette récupération est elle-même résiliente à un crash pendant la
  récupération : une seconde passe repère les runs déjà `Failed` qui ont
  encore un processus ouvert ou un chemin de worktree non nettoyé en base,
  et reprend leur nettoyage — un chemin de worktree n'est effacé en base
  qu'une fois sa suppression effectivement confirmée.
- Sauvegarde automatique de la base SQLite avant toute migration de schéma :
  `db::connect` copie la base existante vers un fichier horodaté
  (`state.db.bak-<horodatage RFC3339>`, suffixé en cas de collision) via
  `VACUUM INTO` (capture fiable même avec des écritures encore uniquement
  dans le WAL) avant d'appliquer les migrations en attente ; un échec de
  cette sauvegarde interrompt la migration plutôt que de continuer sans
  filet de sécurité.
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
- Tolérance aux échecs de polling transitoires (`gh` injoignable, rate
  limit réseau) : jusqu'à 3 échecs consécutifs sont retentés avant abandon,
  le compteur étant réinitialisé dès le prochain poll réussi ; une réponse
  malformée ou inattendue de `gh`, elle, fait toujours échouer `watch-pr`
  immédiatement, sans retry ni tolérance — jamais avalée silencieusement.
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

### Added — Phase 8 : moniteur TUI en lecture seule (`warden-tui`)

- Nouveau binaire `warden-tui`, membre du workspace, sous-commande `attach
  --run-id <ID>` : suit un run en direct depuis un terminal séparé de celui
  qui l'a lancé, **strictement en lecture seule** (ADR-0008) — aucune
  commande d'action (approve/fix/skip) ne transite par lui, il n'écrit
  jamais dans la SQLite de `warden`, ne spawn aucun agent et ne touche
  jamais git ; ces actions restent explicitement hors périmètre v1
  (ADR-0008).
- Event Bus (`warden::event_bus`) : `warden` publie chaque événement
  significatif d'un run (`RunStarted`, `CycleStarted`, `AgentStarted`,
  `AgentFinished`, `FindingRaised`, `RunFinished`) sur un socket Unix
  local propre au run (`~/.warden/runs/<run_id>.sock`, `0600`,
  publish-only — le module ne lit jamais ce qu'un abonné y écrit, donc
  aucune commande ne peut remonter jusqu'à l'orchestrateur par ce canal).
- Nouvelle table `events` (migration `0004_events.sql`) : persiste chaque
  événement publié pour permettre le replay de l'historique complet d'un
  run par une attache tardive.
- `warden-tui` s'abonne au socket **avant** d'interroger l'historique en
  base (ordre déterminant pour éviter tout trou entre replay et direct),
  puis fusionne les deux flux par identifiant d'événement (déduplication) :
  une attache tardive affiche l'historique complet puis bascule sur le
  direct sans perte ni doublon, y compris sur un run déjà terminé (replay
  intégral, sans canal direct).
- Rendu plein écran (`ratatui`) sur un terminal interactif ; sur une sortie
  standard non-TTY (pipe, redirection), bascule automatique vers un flux
  NDJSON (un événement par ligne) — les logs partent toujours sur stderr,
  jamais sur stdout, pour ne pas corrompre ce mode.
- `warden-tui` ouvre la SQLite de `warden` en connexion strictement lecture
  seule et duplique sa propre couche de requêtes plutôt que de dépendre du
  code I/O de `warden`, à l'image de la frontière déjà établie par
  `warden-gated` (ADR-0006).
- Détection des capacités graphiques du terminal (Kitty, iTerm2, Sixel, via
  `ratatui-image`, ADR-0010) et rendu inline de l'evidence quand le
  protocole le permet, avec fallback sur un visualiseur externe sinon.
  **Ce qui n'est pas encore câblé** : la Phase 7 (Evidence Capture Adapter,
  issue #7), qui produirait réellement ces captures, n'est pas encore
  livrée — le type d'événement `EvidenceCaptured` et la table `EVIDENCE`
  n'existent pas encore côté production sur cette branche, donc aucune
  image n'apparaît réellement pour l'instant malgré un rendu fonctionnel et
  testé ; l'extraction de frame vidéo (`ffmpeg`) et la lecture asciinema en
  sous-terminal restent, elles, des erreurs typées explicites
  (`TuiError::NotYetImplemented`), en attendant une source de données
  réelle pour les exercer.
- Cache `sqlx` offline propre au crate (`crates/warden-tui/.sqlx/`),
  indépendant de celui de `warden` et de `warden-gated`.

### Fixed — réparation d'un merge cassé sur `main`

- Un merge antérieur des Phases 7/8 (evidence + événements) avait laissé
  `main` **non compilable** : fonctions tronquées dans
  `crates/warden/src/db.rs`, blocs `use` dupliqués, dépendance `serde`
  perdue, et deux migrations en collision sur le même numéro
  (`0004_events.sql` contre `0004_evidence.sql`). La migration `events` a
  été renumérotée en `0005_events.sql` et le reste du merge corrigé pour
  rétablir un `main` qui compile et dont les tests passent, avant toute
  reprise de travail dessus.

### Added — Issue #15 / ADR-0011 : câblage du CI Watcher dans la boucle de convergence

- `warden` pilote désormais lui-même toute la suite de la state machine
  après `Converged` : `Converged` → `Pushed` (push du commit convergé vers
  le dépôt bare local de `warden-gated`) → ouverture/finalisation
  automatique de la PR (`OpenDraft`/`Finalize`, déclenchées par la nouvelle
  sous-commande `warden-gated run-tail`) → `AwaitingCi` → `Done` /
  `CoderRunning` / `Failed`, selon le résultat terminal remonté par le CI
  Watcher (Phase 5). Auparavant, un run convergé s'arrêtait à `Converged`
  sans suite automatique.
- Nouveau canal de retour : un socket Unix inverse dont `warden` est
  l'écouteur — miroir du relais existant côté hook — sur lequel
  `warden-gated` livre un unique `CiResultMessage` terminal par run.
  `warden` mappe ce message en `CiOutcome`, appelle la fonction pure
  existante `decide_next_state_after_ci`, et écrit lui-même la transition
  qui en résulte : `warden` reste seul writer de son état SQLite,
  `warden-gated` reste strictement en lecture seule (ADR-0006 préservé).
- `ChecksFailed` reboucle vers le coder (`AwaitingCi` → `CoderRunning`) en
  réutilisant la PR déjà ouverte (jamais une seconde ouverture) si le
  budget de cycles le permet, sinon le run passe `Failed`. Aucun merge
  automatique, quel que soit le statut observé.
- Reprise après crash : tout run retrouvé bloqué en `AwaitingCi` au
  redémarrage de `warden` voit sa surveillance CI redemandée à
  `warden-gated` (nouvelle sous-commande `warden-gated resume-watch`)
  plutôt que d'être marqué `Failed` à tort.
- Nouvelle colonne `runs.pr_number` (migration `0006_pr_number.sql`), qui
  permet à la reprise après crash de retrouver la PR d'un run sans que
  `warden-gated` n'ait à conserver le moindre état de surveillance entre
  deux tentatives (GitHub reste la seule source de vérité).
- Les preuves d'exécution capturées (Phase 7, ADR-0009) sont désormais
  transmises jusqu'à la PR finalisée par `run-tail`, qui les fait
  apparaître dans sa section Evidence.
- L'attente côté `warden` du résultat CI est bornée par la durée de vie du
  sous-processus `warden-gated` déclenché (le tail se termine forcément
  quand ce processus sort), et non par un timeout mur-à-mur indépendant.
- Nouveaux flags `--gate-bare-repo`, `--gate-gated-bin`, `--gate-repo-slug`,
  `--gate-poll-interval-secs`, `--gate-inactivity-timeout-secs` sur
  `warden run` : ce câblage est optionnel, son omission préserve le
  comportement antérieur (arrêt à `Converged`).

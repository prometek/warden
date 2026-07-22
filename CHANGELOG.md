# Changelog

Toutes les modifications notables de ce projet sont documentées dans ce fichier.

Le format s'appuie sur [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/),
et ce projet suit [Semantic Versioning](https://semver.org/lang/fr/) une fois publié.

## [Unreleased]

### Added — Issue #53 : visibilité de la consommation de tokens (par agent/cycle/run)

- **`warden_core::TokenUsage`** (pur, sans dépendance outil) : `input_tokens`,
  `output_tokens`, `cache_read_tokens`/`cache_creation_tokens` (`Option<u64>`,
  distincts de `0` — la mise en cache n'est pas rapportée par tous les outils),
  plus `total()`/`merge()`/`sum()`.
- **Nouveau seam optionnel `ToolAdapter::extract_usage(stdout) -> Option<TokenUsage>`**,
  calqué sur `extract_findings` (défaut `None` → « n/a », jamais un `0` fabriqué).
  `ClaudeAdapter` l'implémente en relisant le même envelope `result` déjà capturé
  pour les findings (`claude --output-format stream-json`), sans second parcours
  du flux.
- **Agrégation et persistance** : `Orchestrator::run_agent` accumule l'usage
  extrait sur le total per-rôle du cycle et le total courant du run ; migration
  `crates/warden/migrations/0008_token_usage.sql` (colonnes nullables par rôle
  sur `cycles`, colonnes totales sur `runs`). Publié sur l'Event Bus via
  `RunEvent::AgentFinished { usage: Option<TokenUsage>, .. }`.
- **`warden-tui`** affiche désormais l'usage en direct — par agent, par cycle
  (coder/reviewer/tester) et le total du run — avec repli sur « n/a » tant
  qu'aucun outil n'a rapporté d'usage.

### Added — Issue #55 / ADR-0017 : fondation des hooks de cycle de vie (actions déterministes)

> **Fondation seulement** : types, trait, registre et seam de dispatch. **Aucun
> hook concret** (fmt/test/commit/lint…) et **aucun format de config** ici — ils
> viendront par-dessus. Registre **vide par défaut → comportement strictement
> inchangé** (suite existante verte).

- **Décision (ADR-0017)** : certaines actions d'un run sont **répétables et
  déterministes** (formater, lancer les tests, committer, lint). Les déléguer à
  l'**agent** (dans son prompt) est mauvais sur trois axes — sécurité (l'agent a
  besoin de `Bash`/outillage, ce qui élargit sa surface), tokens (chaque action =
  un aller-retour LLM), déterminisme (un LLM peut oublier, varier, mal exécuter un
  geste mécanique). Ces actions deviennent des **hooks de cycle de vie** exécutés
  **par Warden** aux transitions du run, pas demandés à l'agent. Un hook qui lance
  une commande est prévu pour passer par la **même seam `Sandbox` (#50)** qu'un
  agent — même isolation, mais action fixe, pas de LLM.
- **Types purs dans `warden-core`** (`hook.rs`, aucune dépendance FS/process,
  garde la pureté du crate) :
  - `HookPoint` — enum des moments du cycle de vie (`OnCycleStart`,
    `BeforeCoder`/`AfterCoder`, `OnCommit`, `BeforeReview`/`AfterReview`,
    `BeforeTest`/`AfterTest`, `OnCycleEnd`, `OnConverged`, `BeforePush`), plus
    `HookPoint::on_entering(RunState)` qui mappe l'entrée dans un état sur son
    point de cycle de vie (le mapping qu'utilise le seam de dispatch).
  - `HookContext<'a>` — bundle de **références empruntées** (`run_id`, `state`,
    `cycle`, `worktree`, `commit`, `diff`), aucune possession, aucune I/O ;
    `worktree`/`commit`/`diff` sont `Option` (tous les points ne les portent pas).
  - `HookOutcome` — `Continue` | `Block { reason }` | `EmitFindings(Vec<Finding>)`.
    `EmitFindings` réutilise le `Finding` existant pour alimenter la boucle de
    convergence **comme les findings reviewer/tester/CI (ADR-0011)**, pas un canal
    parallèle.
- **Trait + registre côté `warden`** (`hook.rs`, là où vivent FS/process/sandbox) :
  - `#[async_trait] trait Hook { fn points() -> &[HookPoint]; async fn run(&ctx) -> Result<HookOutcome>; }`.
    Un `Err` = échec réel d'exécution de l'action (propagé) ; un hook qui tourne
    mais veut bloquer/reboucler le dit via `HookOutcome`, jamais via `Err`.
  - `HookRegistry` (Vec plat, **ordre d'enregistrement** = ordre d'exécution
    déterministe). `run_hooks(point, ctx)` agrège : le **premier** `Block`
    court-circuite (arrêt dur) ; sinon les `EmitFindings` sont concaténés dans
    l'ordre ; sinon `Continue`.
- **Câblage du dispatch aux transitions** : `Orchestrator` porte un `HookRegistry`
  (vide par défaut, `with_hooks(...)` pour l'installer) ; `Orchestrator::transition`
  dispatche, après la mise à jour d'état, les hooks du `HookPoint::on_entering(to)`.
  Garde `is_empty()` : registre vide ⇒ le `HookContext` n'est même pas construit,
  comportement strictement inchangé.
- **Hors périmètre, explicite** : **consommer** un `Block`/`EmitFindings` à ce
  seam (gate de politique, injection des findings dans la boucle) est **#51** — en
  attendant, un outcome non-`Continue` est **tracé (`warn!`), jamais silencieusement
  ignoré**, mais pas encore agi ; l'agrégation d'outcome est verrouillée par les
  tests de `HookRegistry::run_hooks`. L'exécution effective dans un sandbox Docker
  est **#49**. La sous-partie des `HookPoint` non mappée sur une entrée d'état
  (`BeforeCoder`/`AfterCoder`, `AfterReview`, `AfterTest`, `OnCommit`,
  `OnCycleEnd`) fait partie du vocabulaire et sera câblée aux sites qui portent
  leur contexte quand les hooks concrets arriveront — sans changement d'enum.
- **Désambiguïsation** : « hook » ici = hook de **cycle de vie** Warden, distinct
  du git `post-receive` de `warden-gated` (`hook.rs` de ce crate).
- **ADR-0017 non écrite dans le vault** : même convention que les
  ADR-0014/0015/0018 — le dossier d'architecture (`docs/`) est un lien symbolique
  non versionné vers un vault Obsidian externe, hors de ce dépôt git. Cette entrée
  de CHANGELOG tient lieu d'enregistrement de décision jusqu'à ce qu'une ADR-0017
  réelle y soit écrite.
- **Tests** : mapping `on_entering` + unicité des chaînes `HookPoint` (core) ;
  no-op du registre vide, tir d'un hook sur son point (pas sur un autre), ordre
  d'enregistrement, court-circuit au premier `Block`, agrégation ordonnée des
  `EmitFindings` (warden) ; et le dispatch réel via `Orchestrator::transition`
  (`transition_dispatches_the_hook_for_the_entered_state`).

### Added — Issue #50 : seam d'isolation de l'environnement d'exécution (`warden-sandbox` + `LocalSandbox`)

- **Nouveau crate `warden-sandbox`** : trait `Sandbox` (`create` → `execute` (N fois)
  → `destroy`), distinct du worktree git (`warden::worktree`, qui isole le *code*) —
  le sandbox isole l'*environnement d'exécution* du process d'un agent, et tourne
  toujours **sur** un worktree, jamais à sa place. `warden-core` ne dépend de ce
  crate dans aucun sens.
- **`LocalSandbox`** : seule implémentation livrée ici, en parité stricte avec
  l'isolation process que `warden::process` appliquait auparavant à la main pour
  chaque coder/reviewer/tester (`env_clear()` + forwarding `PATH`/allowlist,
  `cwd`, `kill_on_drop`, écriture stdin concurrente au drain stdout/stderr, callback
  de progression par ligne, annulation). Un comportement identique, pas un
  changement — le point d'extension pour un futur backend conteneur
  (`DockerSandbox`, issue #49), livré sans lui.
- **`Orchestrator::run_agent`** route désormais chaque invocation coder/reviewer/
  tester via `Arc<dyn Sandbox>` (`LocalSandbox` par défaut), sélectionnable via le
  nouveau `Orchestrator::with_sandbox` — le seul endroit où #49 branchera son
  backend, sans retoucher `run_agent` lui-même. Les erreurs du sandbox sont
  retraduites vers les variantes `ProcessError` existantes pour un texte de CLI
  identique (parité stricte, critère d'acceptation de l'issue).
- **Revue de code — durcissement avant merge** :
  - Le couple `create`/`destroy` d'un sandbox est désormais structurel plutôt que
    positionnel : `SandboxGuard` (RAII) garantit la destruction sur tout retour
    anticipé (`?`) *et* sur l'abandon de la future `run_agent` elle-même
    (annulation de run, sortie de `warden run --tui`) — cas qu'un simple appel
    `destroy()` en fin de fonction ne couvrait pas.
  - `SandboxId` et `Execution` gagnent chacun un constructeur public
    (`SandboxId::new`, `Execution::new`), rendant le trait `Sandbox` réellement
    implémentable hors du crate — sans eux, seul `LocalSandbox` pouvait
    produire les types que `Sandbox::create`/`execute` renvoient, ce qui
    rendait le trait inutilisable comme point d'extension pour #49. Vérifié
    par un test dans `warden` qui installe un faux `Sandbox` enregistreur via
    `with_sandbox` et prouve que `run_agent` route bien
    `create`/`execute`/`destroy` à travers lui, y compris sur un chemin
    d'erreur/annulation ; une suite indépendante ajoutée ensuite (couvrant
    `SandboxGuard::destroy` sur annulation propre, et `kill_on_drop` sur
    abandon de `Execution` sans passer par le chemin d'annulation explicite)
    verrouille les mêmes garanties depuis l'extérieur de l'implémentation.
  - **Limite connue, à traiter par #49** : la récupération après crash
    (`recover_crashed_runs`) continue d'appeler `process::kill_pid` sur un pid
    hôte persisté en base — un raccourci qui ne passe pas par la seam. Sans
    signification pour un futur backend conteneur (le pid hôte du process
    `docker`/`runc` n'est pas le processus agent lui-même) : #49 devra faire
    reposer cette récupération sur `Sandbox::destroy` plutôt que sur un pid nu.
  - **ADR-0015 non écrite** : cette issue en attendait une, mais le dossier
    d'architecture du projet (`docs/`) est un lien symbolique non versionné
    vers un vault Obsidian externe, hors de ce dépôt git — rien à committer ici
    pour la porter. Cette entrée de CHANGELOG en tient lieu d'enregistrement de
    décision jusqu'à ce qu'une ADR-0015 réelle soit écrite dans ce vault, même
    convention que les ADR-0014/ADR-0018 documentées uniquement ici faute de
    dossier `ADR/` versionné dans ce dépôt.
  - Déduplication : `process::wait_with_progress`/`spawn_with_extra_env` (le
    callback de progression par ligne, l'allowlist d'env) sont supprimés —
    devenus du code mort une fois tous les agents passés par cette seam ; seule
    `warden_sandbox::LocalSandbox` porte encore cette logique. `process::spawn`/
    `wait` restent, réduits au strict besoin de l'Evidence Capture Adapter (seul
    appelant restant).
  - `Command::stdin` (le payload `AgentInputMessage` sérialisé — intent + diff
    complet) est désormais exclu d'un `Debug` dérivé (redacté en nombre d'octets),
    et `WardenError::Sandbox` conserve `#[source]` (`#[from] SandboxError`) au
    lieu d'aplatir l'erreur dans un `String`.
### Security — Issue #30 : garde-fou cross-run des définitions d'agent — résolution via l'OS au lieu du matching de chaînes

#### BREAKING — comportement du détecteur `agent_definition_tampering_finding` resserré

- **Le détecteur `touched_agent_definition_paths` (issue #24 review, M4), qui modélisait la
  résolution de chemin par comparaison de chaînes sur `git diff --name-status`, est remplacé
  par un détecteur *resolve-and-compare* : en fin de cycle coder, les trois chemins littéraux
  `.warden/agents/{coder,reviewer,tester}.md` sont **re-résolus à travers l'OS** — via un
  worktree jetable (`git worktree add --detach`) checkouté sur le commit du cycle,
  `agent_def::read_raw_definition`, octets bruts, **jamais parsés** — et comparés à un
  snapshot de démarrage résolu de la même façon sur le commit de départ du run. Toute
  divergence de présence/absence ou de contenu lève le finding bloquant `FindingSource::Warden`
  existant.
- **Ferme les deux contournements documentés depuis #24** (`orchestrator.rs`, revue cycle 3) :
  le folding Unicode `.warden/agentſ/coder.md` (U+017F, que `str::to_lowercase` ratait car déjà
  minuscule alors qu'APFS le fold vers `s`), et le composant parent symlinké
  (`ln -s stash .warden` + `stash/agents/reviewer.md`, dont ni `.warden` ni
  `stash/agents/reviewer.md` ne matchait jamais `AGENTS_DIR` par comparaison de chaînes). Les
  deux côtés de la comparaison posent désormais la même question au même OS — rien à modéliser.
- **Contenu non-parsable → finding, jamais `Err`** : une tentative antérieure inachevée
  (branche `issue-24-m4-resolve-compare-wip`, `823f195`) re-résolvait via
  `resolve_agent_definition`, qui parse — un coder écrivant des octets non-parsables dans une
  définition faisait alors retourner `Err` au détecteur au lieu de lever un finding.
  `read_raw_definition` ne parse jamais et est infaillible ; l'`Err` que la fonction
  peut désormais renvoyer ne couvre plus que l'échec du `git worktree add`/`remove` lui-même,
  jamais le contenu du fichier.
- **Correctif de revue (HIGH) avant tout merge** : la première version de ce correctif
  re-résolvait depuis le *répertoire de travail* du worktree du coder plutôt que depuis le
  commit qui se propage réellement (`new_commit`, ce que `warden-gated` finit par pousser).
  Un coder committant une définition empoisonnée puis faisant `rm -rf .warden` avant de sortir
  laissait le détecteur lire un répertoire de travail nettoyé, identique au snapshot de
  départ — le run convergeait avec le poison dans l'arbre du commit convergé. Corrigé en
  re-résolvant, comme le snapshot de départ, via un worktree jetable checkouté sur le commit
  lui-même — les deux côtés lisent désormais un checkout propre d'un commit, jamais un
  répertoire de travail mutable qu'un process coder encore vivant pourrait modifier (ferme
  aussi une fenêtre TOCTOU résiduelle). Pinné par
  `a_coder_committing_a_poisoned_definition_then_deleting_it_from_the_working_tree_still_blocks` ;
  son symétrique `uncommitted_junk_under_agents_dir_that_never_reaches_the_commit_does_not_block`
  pin le faux positif miroir (du contenu jamais committé sous `.warden/agents/` ne doit jamais
  bloquer, puisqu'il ne peut de toute façon jamais atteindre un run futur).
- **Rétrécissement de comportement, à connaître** (raison du `!` ci-dessus) : sur un système
  de fichiers *sensible à la casse* (Linux ext4 typique), committer
  `.warden/Agents/coder.md` (casse différente) ne lève désormais **plus** de finding — c'est
  correct (l'OS ne résoudrait jamais ce chemin depuis `.warden/agents/coder.md`, donc ce
  n'est pas exploitable là), mais l'ancien détecteur, purement textuel, le signalait
  inconditionnellement quelle que soit la plateforme. Le nouveau détecteur ne signale que ce
  qu'un `warden run` futur lirait réellement sur la plateforme qui l'exécute — même chose sur
  macOS/APFS (défaut insensible à la casse), différent sur un système de fichiers sensible à
  la casse.
- Snapshot de départ toujours ancré sur `run_base_commit_sha` (le vrai commit de départ du
  run, jamais recalculé par cycle) — ce qui reste la bonne sémantique : ce qui peut empoisonner
  un run futur, c'est ce qui est *committé*, pas l'état local du dépôt `--repo` de
  l'utilisateur (qui peut légitimement porter une définition non committée, propre à cette
  seule invocation).
- Détecteur toujours défense en profondeur, pas une garantie : une définition empoisonnée doit
  de toute façon passer une revue humaine de PR pour atteindre un run futur.
- **Indépendant de la confiance asymétrique par rôle introduite par #26 (voir l'entrée
  ci-dessous)** : le détecteur re-résout et compare toujours les trois chemins littéraux
  `.warden/agents/{coder,reviewer,tester}.md`, **sans condition sur `--trust-repo-agents`**
  — y compris pour reviewer/tester, alors même que leur résolution *normale* (via
  `resolve_agent_definition`) ignore désormais ce chemin par défaut depuis #26.
  `--trust-repo-agents` est un réglage *par run* : une définition empoisonnée committée sur
  un run sans le flag reste latente, prête à s'appliquer au premier run futur lancé avec.
  Bloquer inconditionnellement reste fail-closed et préserve les critères d'acceptation de
  #30 tels quels. Le répertoire de configuration utilisateur (la source fiable de #26) est
  volontairement hors du périmètre de ce garde-fou : il vit hors du dépôt, le coder ne peut
  pas y committer, et ce détecteur ne défend que contre ce que le commit du coder change.

### Changed — Issue #26 / ADR-0018 : résolution des définitions d'agent asymétrique par rôle + garde de chemin de programme

**BREAKING** : `<repo>/.warden/agents/{reviewer,tester}.md` n'est plus lu par défaut.

- **Reviewer/tester ne lisent plus le dépôt sous revue par défaut.** Le coder reste
  inchangé (`<repo>/.warden/agents/coder.md` si présent, sinon le défaut de l'adaptateur).
  Reviewer et tester résolvent désormais **uniquement** depuis le répertoire de
  configuration utilisateur — `$XDG_CONFIG_HOME/warden/agents/<role>.md`, ou
  `~/.config/warden/agents/<role>.md` si `XDG_CONFIG_HOME` est absent/vide
  (`agent_def::default_user_config_agents_dir`) — jamais depuis `<repo>/.warden/agents/`.
  Raison : ce dernier est committable par le coder lui-même, et le reviewer/tester
  n'existent que pour le juger de façon indépendante — un coder buggé ou détourné par
  prompt injection pourrait sinon réécrire le prompt (ou les `tools` accordés) du rôle
  censé le contrôler et le faire juger sous ses propres règles à un futur run
  (`warden::agent_def`, section "Security: role-asymmetric resolution").
- **`--trust-repo-agents` (nouveau flag, défaut désactivé)** réautorise
  `<repo>/.warden/agents/<role>.md` comme repli pour le reviewer/tester, mais seulement
  quand aucun fichier n'existe côté configuration utilisateur pour ce rôle (la
  configuration utilisateur a toujours priorité, même avec le flag activé). Quand le flag
  fait effectivement utiliser un fichier du dépôt, c'est surfacé comme non fiable : un
  `tracing::warn!` nommant le chemin, et un nouveau
  `RunEvent::UntrustedAgentDefinitionUsed { role, path, canonical_path }` publié et
  persisté sur le journal d'événements du run (`warden-tui` l'affiche) — changement de
  forme du JSON d'événement persisté. Sans le flag, un fichier repo-contrôlé qui existe
  mais est ignoré émet quand même un `tracing::warn!` nommant le chemin (jamais lu —
  simple `tokio::fs::try_exists`) plutôt que d'être silencieusement abandonné.
- **Le répertoire de configuration utilisateur "fiable" est vérifié, pas supposé.** Un
  `$XDG_CONFIG_HOME`/`~/.config` qui canonicalise à l'intérieur du dépôt sous revue, **ou**
  sous `<warden_home>/worktrees/` (un worktree périmé d'un run précédent), est dégradé au
  même traitement que le repli repo (`AgentDefinitionSource::UntrustedRepoOverride`) plutôt
  que traité en silence comme fiable — sinon un `.envrc`/`devShell` committé par le coder,
  ou un symlink planté, restaurerait exactement le contournement que cette issue ferme,
  avec un journal d'audit affirmant le contraire. Les deux répertoires et le fichier
  `<role>.md` résolu sont canonicalisés (symlinks résolus), pas seulement comparés
  lexicalement.
- **Garde de chemin de programme (belt-and-braces)** : avant de lancer un reviewer/tester,
  `process::validate_agent_program` refuse un `program` relatif, ou un chemin absolu qui
  résout à l'intérieur du dépôt de base, du worktree propre au rôle, ou du worktree de
  **n'importe quel** rôle pour ce run (y compris celui du coder — le répertoire le plus
  contrôlable par le coder qui existe sur disque). Un nom nu résolu via `PATH` reste
  autorisé. Appliquée au point de spawn unique (`Orchestrator::run_agent`), jamais pour le
  coder. Aucun adaptateur livré aujourd'hui ne peut déclencher ce refus (`ClaudeAdapter`
  lance toujours `claude` via `PATH`) — le point 1 de l'issue #26 (`program`/`args`
  relatifs) était déjà rendu sans objet par l'issue #24, qui a supprimé le schéma
  warden-natif `runner`/`program`/`args` ; cette garde est ajoutée en ceinture-et-bretelles
  pour tout futur adaptateur.
- **Limite assumée** : ceci n'est ni un sandbox filesystem ni un sandbox de credentials
  autour du coder — il tourne toujours avec un accès réel au dépôt et les grants par
  défaut de l'adaptateur sélectionné (`Bash` compris). L'isolation réelle reste suivie par
  l'issue #28.
- **Migration** : un projet qui versionnait `reviewer.md`/`tester.md` sous
  `<repo>/.warden/agents/` doit soit les déplacer vers
  `$XDG_CONFIG_HOME/warden/agents/` (ou `~/.config/warden/agents/`), soit passer
  `--trust-repo-agents` pour conserver le comportement précédent (désormais surfacé comme
  non fiable).
- **Correctifs incidents découverts en cours de route** :
  - `WorktreeManager::new`, dont le contrôle de confinement dépôt/`worktrees_root`
    n'échouait pas de façon fermée : il continuait à retirer des segments de chemin sur
    *n'importe quelle* erreur de `canonicalize` (pas seulement `NotFound`), et abandonnait
    silencieusement un échec de `strip_prefix`. Trouvé en extrayant le
    `canonicalize_best_effort` triplement dupliqué (`agent_def.rs`, `process.rs`,
    `worktree.rs`) vers le nouveau module partagé `crate::path_util`, qui fixe l'algorithme
    une fois pour toutes et fait échouer fermé sur toute autre erreur.
  - `main.rs` évaluait `warden_home.unwrap_or(default_warden_home()?)` de façon anticipée,
    ce qui exigeait `HOME` même quand `--warden-home` était explicitement passé.
  - La suite e2e ne surchargeait jamais `XDG_CONFIG_HOME` : une quarantaine de tests
    lisaient le vrai `~/.config/warden/agents/` de la machine qui les exécutait.
- Verrouillé par la suite de tests de `agent_def.rs`, `process.rs` et `path_util.rs`.
- **ADR-0018 (nouvelle)** : voir la note de décision dans le vault de documentation du
  projet.
### Added — Issue #39 : workflow de release CD (binaires prébuilts + GitHub Release)

- Nouveau workflow `.github/workflows/release.yml`, déclenché par le push d'un tag
  `vX.Y.Z` : un job `check-version` échoue si le tag ne correspond pas exactement à la
  version des trois crates binaires (`warden`, `warden-gated`, `warden-tui`) — le tag est
  la seule source de vérité, aucun bump automatique.
- Build `--release` des trois binaires pour trois cibles (`aarch64-apple-darwin`,
  `x86_64-apple-darwin` sur `macos-latest` ; `x86_64-unknown-linux-gnu` sur
  `ubuntu-latest`), en mode `SQLX_OFFLINE` (mêmes caches `.sqlx/` committés que la CI).
- Chaque cible est packagée en `warden-<version>-<target>.tar.gz` (les trois binaires,
  `crates/warden-gated/contrib/{systemd,launchd}`, `README.md`, `LICENSE`) accompagnée
  d'un `.sha256` ; un job final agrège un `checksums.txt` et publie une GitHub Release
  via `gh release create`.
- Nouveau fichier `LICENSE` (MIT) à la racine, embarqué dans chaque archive.
- Toutes les actions tierces utilisées sont épinglées par SHA de commit.
- Voir "Installer depuis une release prébuilt" et "Faire une release (mainteneurs)" dans
  `README.md`.

### Changed — Issue #43 (sous-tâche #37.4) / ADR-0014 : budgets par phase, états par phase, migration DB, `decide_next_state` conscient de la phase

- **Deux budgets indépendants remplacent `max_cycles`** : `RunConfig`/`--max-cycles`
  (CLI) deviennent `max_review_cycles`/`--max-review-cycles` et
  `max_test_cycles`/`--max-test-cycles`, chacun avec sa propre valeur par défaut (5,
  au moins 1). La table `runs` gagne les colonnes `max_review_cycles`,
  `max_test_cycles`, `current_review_cycle`, `current_test_cycle` (migration
  `0007_phase_budgets.sql`, `ALTER TABLE ... ADD COLUMN` + report des valeurs
  existantes + `DROP COLUMN` de `max_cycles`/`current_cycle`) ; `warden-tui`
  (lecture seule, schéma dupliqué par conception) suit le même schéma.
- **Décision #37 Q1, imputation explicite et testée** : `warden_core::decide_next_state`
  ne prend plus un unique `(current_cycle, max_cycles)` mais
  `(review_cycle, max_review_cycles, test_cycle, max_test_cycles)`, et impute chaque
  finding bloquant à son budget selon sa *source* : `Reviewer`/`Warden` (y compris la
  re-review scopée déclenchée par un finding du tester, issue #41/#42) débite le budget
  review ; `Tester` débite le budget test. Une re-review scopée motivée par un finding
  du tester est ainsi imputée au budget review, jamais au budget test — le critère
  d'acceptation central de #43 — verrouillé par
  `a_scoped_re_review_finding_after_a_tester_reboucle_is_charged_to_the_review_budget_not_test`.
- **Budgets réellement indépendants (relecture, correction MEDIUM)** : `run_convergence_loop`
  tient désormais un compteur `review_cycle_number` dédié, distinct du compteur global de
  cycle de la boucle — il n'avance que sur un cycle dont le reboucle est effectivement imputé
  à la review (finding bloquant `Reviewer`/`Warden`), jamais simplement parce que le reviewer
  a tourné ce cycle-là. La première version confondait les deux compteurs (le reviewer
  tournant à chaque cycle, y compris ceux reboucleés par le tester) : un run dont tous les
  reboucles étaient d'origine tester pouvait épuiser à tort son budget review dès le premier
  reboucle review-bloquant venu, même après des dizaines de cycles clean côté review. Persisté
  dans `runs.current_review_cycle`. Verrouillé par
  `max_test_cycles_exceeded_when_tester_findings_never_clear` (budget review minimal `1`,
  jamais épuisé malgré plusieurs reboucles tester) et son symétrique
  `max_review_cycles_exceeded_when_reviewer_findings_never_clear` (budget test minimal `1`,
  jamais épuisé — le tester ne tourne même jamais).
- **Reboucle déclenché par la CI imputé au budget review (relecture, HIGH)** : un reboucle
  `ChecksFailed` (issue #15/ADR-0011) ré-entre dans la boucle en `CoderRunning -> Reviewing`
  comme n'importe quel reboucle review — il débite donc le budget review. Le compteur review
  in-loop n'avance jamais sur un reboucle CI (le code a passé la review localement), donc
  `apply_ci_result_message` incrémente `runs.current_review_cycle` *avant* de statuer, sinon
  une CI durablement rouge boucle indéfiniment sur un compteur qui ne bouge pas au lieu de
  s'arrêter au budget. La boucle relit ce compteur après le reboucle (`review_cycle_number`)
  pour que son écriture review-clean ne l'écrase pas. Sans ce correctif, la première version
  du fix ci-dessus avait désactivé le budget CI (l'ancien compteur global le portait
  auparavant). Verrouillé par
  `repeated_checks_failed_charges_the_review_budget_until_it_terminates_at_failed` (CI rouge
  répétée → `Failed` au budget, `current_test_cycle` jamais touché).
- **États d'épuisement dédiés, jamais un faux `Converged`** : `RunState::MaxCyclesExceeded`
  devient `MaxReviewCyclesExceeded`/`MaxTestCyclesExceeded` — deux états terminaux
  distincts (`-> Failed` uniquement), jamais confondus avec `Converged`.
- **`AwaitingReviewTest` scindé en deux états par phase** : `RunState::Reviewing`
  (coder→reviewer, ouvre la porte Phase A) et `RunState::Testing` (tester, atteint
  uniquement depuis `Reviewing` quand la review du cycle est clean) remplacent l'unique
  `AwaitingReviewTest` qu'utilisaient encore #41/#42. `run_convergence_loop` écrit
  `Reviewing` avant chaque review et `Testing` juste avant chaque passage du tester
  (write-ahead, ADR-0004) ; `RunState::is_intermediate`/crash recovery et
  `db::list_intermediate_runs` suivent les deux nouveaux états.
- **Migration, remap des valeurs `state` historiques (relecture, LOW)** : au-delà des colonnes
  numériques, `0007_phase_budgets.sql` remappe aussi les valeurs `awaiting_review_test` →
  `reviewing` et `max_cycles_exceeded` → `max_review_cycles_exceeded` sur les lignes
  existantes — sans ce remap, un run persisté dans l'un de ces états au moment de la mise à
  jour serait devenu `UnknownState` et n'aurait plus jamais été repris par la récupération
  après crash. L'attribution exacte de phase n'est pas reconstructible depuis la seule chaîne
  `state` ; le choix (conservateur pour `reviewing`, cosmétique pour
  `max_review_cycles_exceeded`) est documenté dans la migration elle-même.
- Aucun changement de comportement pour la boucle Phase A/B elle-même (héritée de
  #41/#42) : seuls les budgets/états qui l'entourent deviennent conscients de la phase.

### Changed — Issue #42 (sous-tâche #37.3) / ADR-0014 : Phase B — gate test avec re-review scopée avant tout retour au tester

- **Un finding du tester reboucle vers le coder exactement comme un finding du reviewer** :
  `decide_next_state` (`crates/warden-core/src/convergence.rs`) traite déjà tout finding
  bloquant de façon uniforme, quelle que soit sa source — ce qui signifie que la gate de
  re-review scopée livrée par #41 (`has_reviewed_once`/`ReviewScope::Correctif`) rejouait déjà,
  sans code supplémentaire, exactement le comportement demandé par #42 : la correction du coder
  pour un finding du tester repasse par la même re-review scopée (portant sur le correctif et
  les findings du tester qui l'ont motivé, décision #37 Q2) avant que le tester ne soit jamais
  rappelé sur ce commit. Si cette re-review relève à son tour un finding (p. ex. une régression
  introduite par le correctif), le cycle reboucle vers le coder sans jamais rappeler le tester,
  et continue ainsi coder↔reviewer jusqu'à ce que la review soit clean — la convergence n'est
  atteinte que quand le tester lui-même est clean (invariant #42 : "aucun code non-revu
  n'atteint le tester"). Aucun changement de `run_convergence_loop` n'a donc été nécessaire ;
  seuls les commentaires de `crates/warden/src/orchestrator.rs` (module + gate review/gate test)
  ont été mis à jour pour documenter explicitement ce comportement au lieu de le décrire comme
  "pas encore livré".
- Deux nouveaux tests verrouillent ce comportement comme critère d'acceptation explicite de #42
  plutôt que comme simple effet de bord de la généralisation de #41 :
  `a_tester_finding_reboucles_through_a_scoped_re_review_before_the_tester_reruns` (un finding du
  tester déclenche une correction, une re-review scopée à ce finding, puis un nouveau passage du
  tester qui converge) et
  `a_scoped_reviewer_finding_on_the_correctif_reboucles_again_before_the_tester_reruns` (le
  correctif du coder introduit lui-même une régression relevée par la re-review scopée : le
  cycle reboucle vers le coder une deuxième fois — sans jamais rappeler le tester — avant que la
  review ne soit clean et que le tester ne reprenne la main).
- **Limite connue, héritée de #43 non livrée** : le budget des re-reviews scopées n'est pas
  imputé à un budget review séparé d'un budget test — ce dépôt ne porte encore qu'un unique
  `max_cycles` et un unique `RunState::AwaitingReviewTest`, partagés entre les deux gates (état
  et budget par phase : #43). Aucune migration ni état supplémentaire n'a été ajouté ici pour
  rester dans le périmètre de #42.

### Changed — Issue #41 (sous-tâche #37.2) / ADR-0014 (nouvelle) : Phase A — boucle de gate review (coder↔reviewer jusqu'à clean)

- **Le tester est désormais gated derrière une review clean** : `run_convergence_loop`
  lance `run_review` en premier à chaque cycle, et n'appelle `run_test` que si ce
  cycle-là ne porte aucun finding bloquant (reviewer + le finding de trafiquage de
  définition existant, issue #24 M4). Un cycle dont la review n'est pas clean reboucle
  directement vers le coder — le tester ne tourne jamais sur du code pas encore validé
  par le reviewer (issue #41, critère d'acceptation).
- **Re-review scopée après la première** : le premier passage du reviewer sur le corps
  de travail d'un run est complet (`ReviewScope::Full`, tout le diff) ; chaque
  ré-invocation suivante — après une correction du coder — est scopée
  (`ReviewScope::Correctif`, décision #37 Q3) au correctif plus les findings qui l'ont
  motivé, via le payload `AgentInputMessage`/`ReviewScope` introduit par #40. Suivi sur
  toute la durée du run (`has_reviewed_once`), jamais remis à zéro par cycle.
- **ADR-0014 (nouvelle, issue #37)** : remplace le parallélisme réel review/test
  d'ADR-0003 par une convergence en **deux phases à portes** — Phase A : boucle
  coder↔reviewer jusqu'à review clean (livrée ici) ; Phase B (#42, pas encore livrée) :
  boucle tester→coder→**re-review scopée**→tester jusqu'à tester clean, sans jamais
  laisser de code non revu atteindre le tester. Budgets séparés par phase et états dédiés
  (`decide_next_state` conscient de la phase) restent à câbler par #43 — cette livraison
  garde volontairement un budget/état unique (`max_cycles`, `RunState::AwaitingReviewTest`)
  pour rester dans le périmètre de #41. **Cette entrée de CHANGELOG est, pour l'instant,
  le seul enregistrement écrit de la décision ADR-0014** (pas de fichier `ADR-0014.md`
  séparé — ce dépôt n'a pas de dossier `ADR/` versionné, même convention que les
  amendements ADR-0003/ADR-0012/ADR-0013 documentés uniquement ici par #40) ; le
  write-up complet (détails Phase B, budgets/états par phase) suivra au fil de #42/#43.
- Deux nouveaux tests couvrant le critère d'acceptation : le tester ne tourne jamais
  tant que la review n'est pas clean (compteur d'invocations dédié), et la première
  review est complète tandis que la ré-review qui suit une correction est scopée
  (payload stdin capturé et reparsé).

### Changed — Issue #40 (sous-tâche #37.1) / ADR-0003, ADR-0012, ADR-0013 (amendements) : `run_review`/`run_test` indépendantes + reviewer scopé (fondations)

- **Suppression de `run_review_and_test`** (le `tokio::join!` reviewer+tester,
  ADR-0003) : `run_review` et `run_test` sont désormais deux fonctions
  indépendantes, chacune avec son propre worktree, son propre spawn d'agent
  et sa propre gestion des findings — testées isolément. `run_convergence_loop`
  les appelle **séquentiellement** (reviewer puis tester), une étape
  intermédiaire vers la boucle à deux phases (issue #37) qui supprime au
  passage le risque de collision de tokens entre les deux agents tournant en
  concurrence sur le même commit.
- **Reviewer scopé** : `AgentInputMessage` porte un nouveau champ `scope`
  (`"full"` ou `"correctif"`) permettant d'invoquer le reviewer en mode
  « regarde uniquement ce correctif » — `diff`/`findings` portent alors le
  correctif du coder et les findings qui l'ont motivé (décision #37 Q2), au
  lieu du contexte complet du cycle. Nouveau constructeur
  `AgentInputMessage::for_scoped_review`, réservé au rôle reviewer (rejeté en
  écriture comme en lecture pour tout autre rôle).
- `AGENT_INPUT_VERSION` passe à **3** : un payload v2 (sans champ `scope`) est
  refusé explicitement, jamais lu en silence comme `scope: "full"` — même
  convention de rétro-compatibilité que le passage 1 → 2 (ADR-0013).
- Prompts par défaut du reviewer (`ClaudeAdapter`) mis à jour pour documenter
  `scope` et le comportement attendu en mode `"correctif"`.
- Ne construit pas encore la nouvelle boucle à deux phases (états par phase,
  budgets séparés, re-review scopée automatique) : voir les sous-tâches
  suivantes de #37 (#41, #42, #43).

### Added — Issue #32 / ADR-0008 (amendement) : `warden run --tui`, lancement automatique de la TUI

- Nouveau flag `--tui` sur `warden run` : dès que le run démarre, `warden` lance
  `warden-tui attach --run-id <id> --warden-home <path>` comme **process séparé**, au
  premier plan sur le terminal de lancement — le flux "je lance et je regarde" sans avoir
  à copier la commande `warden-tui attach` affichée au démarrage dans un second terminal.
- Nouveau flag `--tui-bin <PATH>` (ignoré sans `--tui`) pour surcharger le binaire
  `warden-tui` lancé ; par défaut, recherché à côté du binaire `warden` en cours
  d'exécution, avec repli sur une résolution via `PATH`.
- **Quitter la TUI, pour quelque raison que ce soit (`q`, `Esc`, Ctrl-C, ou un crash),
  annule le run** : la TUI reste strictement en lecture seule (ADR-0008), il n'existe
  aucun canal retour pour distinguer "détache-toi, laisse le run continuer" de "annule" —
  sa sortie est le seul signal disponible, traité uniformément. `warden-tui` traite
  désormais Ctrl-C comme une touche de sortie au même titre que `q`/`Esc`, puisque le
  mode raw de son terminal empêche Ctrl-C de générer un `SIGINT`.
- Sur un terminal de lancement interactif (TTY), `warden run` attend que la TUI se
  termine avant de rendre la main, pour qu'elle restaure proprement le terminal. Sur une
  sortie standard non-TTY (`warden run --tui > events.ndjson`, pipe, `tee`), `warden-tui`
  bascule sur son mode texte NDJSON existant et `warden run` n'attend pas.
- Un échec de spawn de `warden-tui` (binaire introuvable, erreur d'exécution) annule
  immédiatement le run avec une erreur explicite nommant le chemin résolu, plutôt que de
  dégrader silencieusement vers un run headless.

### Added — Issue #33 / ADR-0008 (amendement) : progression d'agent en direct dans la TUI

- Entre `AgentStarted` et `AgentFinished`, la TUI restait aveugle pendant toute la durée
  d'un agent. `ClaudeAdapter` lance désormais `claude` avec
  `--output-format stream-json --verbose` (au lieu de `--output-format json`) : Claude Code
  émet ainsi lui-même ses événements au fil de l'eau (messages assistant complets, blocs
  `tool_use`), avant de terminer par la même enveloppe `{"type":"result", ...}` qu'avant —
  `ClaudeAdapter::extract_findings` continue de fonctionner à l'identique, en cherchant
  cette enveloppe sur la **dernière ligne non vide** de stdout plutôt que sur la totalité
  du buffer.
- Nouveau variant `RunEvent::AgentProgress { role, detail }` (`EventKind::AgentProgress`),
  traduit ligne par ligne depuis la sortie streamée de l'agent par le nouveau seam
  `ToolAdapter::parse_progress_line` (`warden::tool_adapter`) — la spécificité du format
  d'un CLI donné (`stream-json` pour `claude`) n'est jamais exposée à `warden_core` ni à
  `warden-tui`, qui ne voient qu'une `String` déjà traduite. `process::wait_with_progress`
  remplace la lecture bloc-à-bloc de stdout par une lecture ligne par ligne, en conservant
  la même garantie anti-deadlock (drainage stdin/stdout/stderr/wait concurrent).
- **Live-only, jamais persisté** : `Orchestrator::publish_progress_event` diffuse
  `AgentProgress` sur l'Event Bus mais ne l'écrit jamais dans la table `events` —
  contrairement à tout autre `RunEvent`, dont la persistance reste intégrale et inchangée.
  C'est un signal d'observation temps réel, pas un élément d'audit : l'evidence (ADR-0009)
  reste la seule source qui porte une valeur de preuve. Conséquence assumée : une attache
  tardive de la TUI ne rejoue jamais la progression détaillée d'un agent déjà en cours ou
  déjà terminé, seul un abonné connecté au moment de la publication la voit.
- `warden-tui` affiche cette progression en direct (rôle + dernier detail rapporté) tant
  qu'un agent est actif, et l'efface dès `AgentFinished` ; le flux NDJSON (mode non-TTY)
  et l'historique d'événements affichent aussi `AgentProgress` au même titre que les autres
  événements, en le distinguant visuellement des événements de cycle de vie.
- Granularité retenue : messages assistant complets et blocs `tool_use`, délibérément
  **sans** `--include-partial-messages` (chunks de tokens), jugé trop fin pour l'usage visé.
- À ne pas confondre avec l'evidence (ADR-0009) : la progression d'agent est **déclarative**
  (ce que l'agent *rapporte* faire, via son propre CLI), jamais une preuve vérifiée
  d'exécution — cette dernière reste le rôle exclusif de l'Evidence Capture Adapter.

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

### Added — Issue #20 Scope B / ADR-0012 : propagation de l'intent/contexte aux agents via stdin JSON

- Chaque agent lancé par `warden` (`--coder-cmd`/`--reviewer-cmd`/`--tester-cmd`, inchangés —
  aucun nouveau flag CLI dans cette livraison) reçoit désormais sur son `stdin` un payload
  JSON versionné (`warden_core::AgentInputMessage`), puis `stdin` est fermé (EOF) : le coder
  reçoit l'intent du run ; le reviewer et le tester reçoivent le commit cible, le diff du
  cycle et les findings qui ont déclenché ce cycle (y compris les findings CI injectés sur un
  reboucle post-convergence, ADR-0011). Auparavant l'intent n'atteignait le coder par aucun
  canal géré par Warden (l'utilisateur devait l'embarquer hors-bande dans sa commande), et le
  reviewer/tester ne recevaient ni commit, ni diff, ni findings.
- `process::spawn`/`process::wait` pipent désormais stdin en plus de stdout/stderr ; l'écriture
  du payload, le drain de stdout/stderr et l'attente de fin de process tournent concurremment
  pour éviter un deadlock si un agent entrelace lecture de stdin et écriture de sortie
  volumineuse. `env_clear()` (seul `PATH` transmis) reste inchangé — l'intent ne transite
  jamais par une variable d'environnement ni par un argument de ligne de commande.
- Un échec d'écriture stdin autre qu'une pipe cassée (agent qui ferme ou n'ouvre jamais son
  côté lecture — cas légitime, non fatal) fait désormais échouer l'invocation
  (`ProcessError::StdinWrite`) plutôt que de laisser tourner silencieusement un agent qui n'a
  jamais reçu son payload.
- Le diff transmis au reviewer/tester est borné à 8 Mio (`MAX_DIFF_BYTES`), mémoire réellement
  bornée (le surplus est drainé vers `tokio::io::sink()`, jamais rebufferisé) ; un diff tronqué
  porte un marqueur explicite (`DIFF_TRUNCATED_MARKER`) que l'agent peut détecter. `git diff`
  est invoqué avec `--no-color --no-ext-diff --no-textconv -c color.ui=false` et un séparateur
  `--`, pour empêcher la configuration git du dépôt ou de l'utilisateur (dont un `textconv`
  défini via `.gitattributes`) de corrompre ou de masquer le diff reçu par l'agent.
- `--intent` est désormais validé (rejet d'une valeur vide/blanche) dès la frontière CLI
  plutôt qu'en profondeur du premier cycle, une fois la ligne `runs` déjà créée.
- Hors périmètre de cette livraison : le scope A de l'issue #20 (agents définis par fichier
  markdown, seam de runner pluggable, flags `--*-agent`) ; aucun timeout par invocation
  d'agent (différé, documenté dans les Conséquences d'ADR-0012) ; le coder ne reçoit toujours
  pas les findings du cycle précédent qu'il est censé corriger, seul l'intent du run l'est
  (lacune connue, également documentée dans ADR-0012).

### Changed — Issue #22 / ADR-0013 : agents définis par fichier markdown (seam de runner) + findings au coder

#### BREAKING (CLI) — `--coder-cmd`/`--reviewer-cmd`/`--tester-cmd` supprimés

- Un rôle n'est plus une chaîne shell découpée sur les espaces : il est décrit par un
  **fichier markdown** (frontmatter TOML fencé par `+++`, puis le corps markdown = le
  **system prompt** du rôle). Les flags `--coder-cmd`/`--reviewer-cmd`/`--tester-cmd`
  sont **supprimés** (migration complète annoncée dans ADR-0012, Q4) et remplacés par
  `--coder-agent`/`--reviewer-agent`/`--tester-agent`, qui prennent le chemin d'une
  définition. `parse_agent_command` (et son découpage naïf sur les espaces, qui
  mutilait tout argument contenant une espace) disparaît avec eux.
- **Migration** — chaque `--*-cmd` devient une définition markdown pointée par le
  `--*-agent` correspondant. Aucune capacité perdue : le runner `command` est un
  échappatoire de première classe (programme + arguments bruts), donc un simple script
  reste une cible valide.

  Avant :

  ```sh
  warden run --repo . --intent "..." \
    --coder-cmd "claude -p coder.md" \
    --reviewer-cmd "sh ./reviewer.sh" \
    --tester-cmd "sh ./tester.sh"
  ```

  Après — `agents/coder.md` :

  ```markdown
  +++
  runner = "command"
  program = "claude"
  args = ["-p"]
  +++

  Tu es le coder de Warden. Lis le payload JSON sur stdin (`intent`, `findings`),
  implémente la tâche et commite dans le worktree courant.
  ```

  ```sh
  warden run --repo . --intent "..." \
    --coder-agent agents/coder.md \
    --reviewer-agent agents/reviewer.md \
    --tester-agent agents/tester.md
  ```

  Note : les arguments sont désormais une **liste explicite** (`args = ["-p", "mon
  fichier.md"]`), plus une chaîne découpée sur les espaces.

#### Autres changements

- Nouveau schéma **warden-natif** (`warden_core::parse_agent_definition`), délibérément
  pas le format `.claude/agents/*.md` de Claude Code : l'adopter coupleraient Warden à
  un CLI d'agent précis et casserait l'agnosticisme d'agent (ADR-0005). Validation à la
  frontière avec la même rigueur que `parse_agent_input_message` : clé inconnue
  (`deny_unknown_fields`), **clé d'un autre runner** (les clés sont scopées à leur runner,
  parsées en deux passes — sélecteur `runner` puis structure propre au runner — pour
  qu'une clé destinée à un autre runner soit une erreur typée et non acceptée-puis-ignorée),
  runner inconnu, `program` manquant/vide, fence absente ou non fermée, fichier CRLF ou
  préfixé d'un BOM (erreur nommant la vraie cause, la fence n'étant pas en tort), ou system
  prompt vide/blanc → erreur typée, jamais de valeur par défaut silencieuse.
- Nouveau seam de **runner** (`warden::agent_runner::AgentRunner`), trait résolu à la
  compilation sur le modèle de `GateTrigger` : il reçoit la définition parsée et renvoie
  la commande à lancer. `CommandRunner` est l'implémentation de production.
  `run_convergence_loop` prend désormais un runner en paramètre générique.
- **Payload agent v2** (`AGENT_INPUT_VERSION` : 1 → 2, breaking pour un consommateur
  côté agent) : chaque payload porte désormais `system_prompt` (le corps markdown de la
  définition du rôle), transmis sur stdin — jamais par argv (fuite dans `ps`) ni par un
  fichier de prompt temporaire, exactement les canaux qu'ADR-0012 avait déjà écartés.
- **Le coder reçoit enfin les findings qu'il doit corriger** (lacune documentée dans les
  Conséquences d'ADR-0012) : sur un reboucle, `AgentInputMessage::for_coder` porte
  l'intent **et** les findings du cycle précédent (y compris les findings CI d'un
  reboucle post-convergence, ADR-0011) — la même liste que reçoivent le reviewer et le
  tester (`select_prior_findings`, inchangé). Toujours **ni `target_commit` ni `diff`**
  pour le coder : son worktree est déjà checkouté sur ce commit, il peut faire son
  `git diff` lui-même. `for_coder` refuse toujours un intent vide/blanc, et
  `parse_agent_input_message` **rejette** désormais un payload coder qui porterait un
  `target_commit`/`diff` (en nommant le champ) plutôt que de l'écarter en silence — « intent
  + findings seulement » est un invariant, donc validé aussi à la lecture.
- Note de sécurité (documentée, non contrainte) : un `program`/`args` relatif dans une
  définition se résout contre le worktree du rôle, un checkout du dépôt sous revue —
  `program = "./reviewer.sh"` exécute donc du code que le coder peut committer. Chemin
  absolu recommandé pour reviewer/tester (README, `warden_core::RunnerKind`, ADR-0013).
- **Hors périmètre, inchangé** : aucun timeout par invocation d'agent (la définition
  markdown n'expose **pas** de clé `timeout` — une clé acceptée ici l'implémenterait à
  moitié ; ticket dédié) ; aucun auto-merge ni changement de la frontière credentials
  (ADR-0002/0006) ; Warden ne livre toujours aucune implémentation d'agent (ADR-0005).

### Changed — Issue #24 : modèle d'adaptateur `--tool` + format d'agent Claude Code + lancement simplifié

#### BREAKING (CLI) — `--coder-agent`/`--reviewer-agent`/`--tester-agent` supprimés, le schéma de définition warden-natif aussi

- Le schéma de définition `+++`/TOML warden-natif introduit par #22 (ADR-0013) s'est révélé
  trop coûteux à câbler en pratique pour un vrai run : il fallait écrire trois fichiers
  `.md`, plus des scripts wrapper faits main pour restaurer `HOME` dans l'environnement de
  l'agent (`env_clear()` ne laissait passer que `PATH`) et pour traduire la sortie du CLI en
  NDJSON de findings. Les flags `--coder-agent`/`--reviewer-agent`/`--tester-agent` et le
  seam de runner `warden::agent_runner::AgentRunner`/`CommandRunner` qu'ils sélectionnaient
  sont **supprimés**, ainsi que le schéma `+++`/TOML lui-même (`RunnerKind`,
  `CoreError::UnknownRunner`).
- Remplacés par **`--tool <name>`** (obligatoire, ensemble fermé résolu à la compilation —
  `claude` seul pour l'instant) qui sélectionne un **adaptateur d'outil intégré**
  (`warden::tool_adapter::ToolAdapter`, `ClaudeAdapter`) pour les trois rôles du run.
  L'adaptateur construit l'invocation réelle du CLI, déclare l'allowlist d'environnement
  dont il a besoin, traduit lui-même la sortie de l'outil en NDJSON de findings, et fournit
  un prompt et un `tools` par défaut par rôle — l'utilisateur n'écrit plus de wrapper.
- Le format de définition d'un rôle adopte directement celui de **Claude Code**
  (`.claude/agents/*.md` : frontmatter **YAML** `---`, clés `name`/`description`/`tools`/
  `model`, corps markdown = system prompt), remplaçant le schéma warden-natif `+++`/TOML.
- **Définitions par convention, plus par flag obligatoire** : `<repo>/.warden/agents/
  {coder,reviewer,tester}.md`, si présent — sinon le prompt/`tools` par défaut de
  l'adaptateur sélectionné. Résultat : un run tourne avec **zéro fichier `.md`**.

  Avant (#22/#23) :

  ```sh
  warden run --repo . --intent "..." \
    --coder-agent agents/coder.md \
    --reviewer-agent agents/reviewer.md \
    --tester-agent agents/tester.md
  ```

  Après :

  ```sh
  warden run --repo . --intent "..." --tool claude
  ```

  **Migration** — deux options pour un rôle dont vous voulez garder le contrôle plutôt que
  le défaut de l'adaptateur : convertissez chaque définition `+++`/TOML existante en
  frontmatter YAML `---` (`name`/`description`/`tools`/`model`, corps = system prompt
  inchangé) sous `.warden/agents/<role>.md` du dépôt cible ; ou abandonnez-la et laissez
  l'adaptateur fournir son prompt/`tools` par défaut pour ce rôle.

#### Sécurité — à connaître avant de lancer un run

- **`ClaudeAdapter` accorde `Bash` par défaut** aux trois rôles (coder :
  `Read, Write, Edit, Bash` ; reviewer : `Read, Grep, Glob, Bash` ; tester :
  `Read, Write, Edit, Grep, Glob, Bash`), et **`HOME`/`USER` sont désormais transmis** à
  l'agent (allowlist d'environnement explicite par adaptateur, `USER` requis en pratique
  pour l'auth `claude` via le trousseau macOS) là où seul `PATH` l'était auparavant. En
  déploiement `warden`/`warden-gated` même-UID (le défaut documenté), un agent qui tourne
  normalement — pas seulement un `warden` compromis — a donc les moyens d'atteindre la clé
  SSH réelle de l'utilisateur et `~/.config/gh`, et de pousser directement vers `origin` en
  contournant `warden-gated`. Documenté honnêtement dans `docs/Architecture.md` (ADR-0006,
  amendement issue #24 ; §10) plutôt que présenté comme déjà couvert. Isolation réelle
  trackée par l'issue #28.
- Le détecteur de trafiquage de définition inter-run (`.warden/agents/` touché par un diff
  coder → finding bloquant) est une défense en profondeur, **pas une garantie** : deux
  contournements sont connus et documentés (`orchestrator.rs`, issue #30).

#### Autres changements

- Correctif de sécurité en cours de revue de cette même issue : `AsciinemaAdapter`
  citait naïvement (espace-join) la commande enregistrée avant de la passer à
  `asciinema rec --command`, qui l'exécute via un shell. `ClaudeAdapter::build_command`
  met désormais le system prompt complet dans les args (`--append-system-prompt`), et les
  prompts par défaut contiennent des métacaractères shell (apostrophes, backticks) —
  un espace-join naïf y devenait un vecteur d'injection shell. Chaque partie de la commande
  est désormais quotée individuellement (`shlex::try_quote`) avant l'assemblage.

### Added — Issue #31 : `warden run` imprime le `run_id` et la commande d'attache dès le démarrage

- `warden run` imprime désormais sur stdout, **au démarrage du run** (visible à la
  verbosité par défaut, sans `-v`), le `run_id` et une commande `warden-tui attach`
  prête à copier :

  ```
  run 5f4d6e3a-... started
  attach: warden-tui attach --run-id 5f4d6e3a-... --warden-home /Users/alice/.warden
  ```

  Auparavant le `run_id` n'apparaissait qu'à la toute fin du run (`run <id> finished:
  ...`), obligeant à requêter la table `runs` en SQLite à la main pour attacher la TUI
  à un run encore en cours. Le `--warden-home` imprimé est résolu en chemin absolu et
  quoté (`shlex::try_quote`), pour rester copiable tel quel même si `--warden-home` a
  été passé sous forme relative ou contient un espace.

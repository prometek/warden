# Warden

Orchestrateur local, écrit en Rust, qui pilote un cycle de développement assisté par
plusieurs agents IA spécialisés (coder, reviewer, tester, rédacteur de doc) jusqu'à
convergence, puis livre le résultat via un point de passage git indépendant du jugement
des agents.

## État du projet

Phase 1 (fondations), Phase 2 (parallélisme réel), Phase 3 (gate git) et Phase 8 (TUI)
sont livrées : un workspace Cargo avec le binaire `warden` (orchestrateur), capable de
piloter une boucle de convergence (coder → review/test → reboucle si besoin) avec
persistance SQLite et reprise après crash : au redémarrage, tout run laissé dans un état
intermédiaire sans processus agent vivant est marqué `Failed`, et les ressources qu'il a pu
laisser orphelines (worktrees git, processus agents encore en vie) sont automatiquement
récupérées — y compris si un second crash interrompt la récupération elle-même. Une
sauvegarde de la base SQLite est également prise avant toute migration de schéma.
Reviewer et tester tournent **en parallèle** (`tokio::join!`), chacun
dans son propre worktree synchronisé sur le commit du coder. Un second binaire,
`warden-gated`, forme désormais la frontière de sécurité vers le remote réel
(ADR-0002/ADR-0006) : il ne partage aucun code I/O avec `warden`, relit lui-même l'état du
run et le hash validé en SQLite (connexion strictement lecture seule) avant tout push vers
`origin`, et ne fait jamais confiance à ce que `warden` prétend. Un troisième binaire,
`warden-tui`, permet de suivre un run en direct depuis un terminal séparé, strictement en
lecture seule (ADR-0008) — voir "`warden-tui` (moniteur en lecture seule)" ci-dessous.

**Câblage complet post-convergence (issue #15/ADR-0011)** : `warden` pilote désormais
lui-même toute la suite de la state machine après `Converged` — il pousse le commit
convergé vers le dépôt bare local de `warden-gated` (`Converged` → `Pushed`), déclenche en
sous-processus `warden-gated run-tail`, qui ouvre/finalise la PR (`OpenDraft`/`Finalize`,
voir "Gestion des PR" ci-dessous) puis surveille la CI (`watch_pr`, voir "CI Watcher"
ci-dessous) jusqu'à un statut terminal. Ce résultat est renvoyé à `warden` par un socket
Unix inverse (`warden` écouteur, `warden-gated` émetteur) ; `warden` le mappe via la
fonction pure `warden_core::decide_next_state_after_ci` et écrit lui-même la transition qui
en découle (`AwaitingCi` → `Done` / `CoderRunning` / `Failed`) — il reste le seul writer de
son état SQLite, `warden-gated` reste en lecture seule (ADR-0006). Un `ChecksFailed`
reboucle vers le coder en réutilisant la PR déjà ouverte (jamais une seconde ouverture) si
le budget de cycles le permet, sinon le run passe `Failed`. Un run resté bloqué en
`AwaitingCi` après un crash de `warden` voit sa surveillance CI redemandée automatiquement
au redémarrage plutôt que d'être marqué `Failed` à tort. Ce câblage est optionnel : sans
`--gate-bare-repo`/`--gate-gated-bin` (voir "Flags de `warden run`" ci-dessous), un run
s'arrête toujours à `Converged`, comme avant cette livraison. **Aucun merge automatique** :
la décision de merger reste entièrement humaine, quel que soit le statut observé.

Le **PR Manager** (`OpenDraft` / `PostCycleUpdate` / `Finalize`) — voir "Gestion des PR"
ci-dessous — a ses actions `OpenDraft`/`Finalize` désormais invoquées automatiquement par ce
câblage ; `PostCycleUpdate` reste, elle, une capacité de bibliothèque non encore invoquée
automatiquement par `warden`.

Le **CI Watcher** (Phase 5) — voir "CI Watcher" ci-dessous — est désormais invoqué
automatiquement par ce même câblage plutôt qu'uniquement via la sous-commande de diagnostic
`warden-gated watch-pr`, qui reste disponible pour rejouer une surveillance indépendamment
de tout run.

L'**Evidence Capture Adapter** (Phase 7, ADR-0009) est livré et entièrement câblé dans la
boucle de convergence de `warden` : chaque cycle dont le tester réussit son test e2e produit
une preuve (Playwright ou asciinema selon le projet), committée dans le dépôt à la
convergence si `--evidence-store-in-repo` (défaut) — voir "Preuve d'exécution (Evidence)"
ci-dessous. Le renderer de la section Evidence du corps de PR est lui aussi livré et appelé
par `warden-gated::pr_manager::finalize` ; `Finalize` étant désormais déclenché
automatiquement à la convergence (issue #15/ADR-0011, voir ci-dessus), cette section
Evidence apparaît réellement dans la PR finalisée d'un run convergé avec
`--evidence-store-in-repo` (défaut) et le câblage `--gate-*` activé.

**Limite d'isolation à connaître avant tout déploiement** : dans la configuration par
défaut documentée ici, `warden` et `warden-gated` tournent sous le **même utilisateur OS**.
Cela donne une frontière de sécurité **process/logique** (aucun code d'accès credentials
partagé, revérification indépendante en base) mais **pas** une isolation OS — un `warden`
compromis au niveau code, sous cet UID, peut lire directement les credentials `origin`. Voir
"Déploiement durci" ci-dessous et ADR-0006 dans `docs/Architecture.md` pour la configuration
qui ferme cet écart.

**Depuis l'issue #24, ce n'est plus seulement un `warden` compromis qui pose problème.**
L'adaptateur `claude` (`--tool claude`) accorde `Bash` par défaut aux trois rôles et
transmet `HOME`/`USER` à l'agent — un agent qui tourne **normalement** a donc les moyens
d'atteindre la clé SSH réelle de l'utilisateur et `~/.config/gh`, et de pousser directement
vers `origin` en contournant `warden-gated` entièrement, sous le même déploiement même-UID.
Documenté sans l'euphémiser dans `docs/Architecture.md` (ADR-0006, amendement issue #24 ;
§10). Isolation réelle côté agent — pas seulement côté gate — trackée par l'issue #28.

## Structure du dépôt

- `crates/warden-core/` — logique pure (state machine des runs, interprétation des
  findings), 100 % testable sans I/O.
- `crates/warden/` — binaire orchestrateur (`[[bin]] warden`) : CLI, gestion des
  worktrees git, spawn des agents en sous-processus, persistance SQLite (`sqlx`),
  boucle de convergence.
- `crates/warden-gated/` — binaire du gate git (`[[bin]] warden-gated`) : seul détenteur
  des credentials vers `origin`, hook `post-receive` minimal + revérification indépendante
  de l'état avant tout push (voir "Le gate git (`warden-gated`)" ci-dessous).
- `crates/warden-tui/` — binaire du moniteur en lecture seule (`[[bin]] warden-tui`) :
  s'abonne à l'Event Bus de `warden` et relit la table `events` en SQLite (connexion
  strictement lecture seule), sans jamais écrire en base, spawn d'agent, ni accès git (voir
  "`warden-tui` (moniteur en lecture seule)" ci-dessous).

## Compilation & tests

```sh
cargo build
cargo test
```

Ces commandes fonctionnent **hors ligne**, sans base de données ni `DATABASE_URL` : les
requêtes `sqlx` sont vérifiées à la compilation via les caches `.sqlx/` committés dans
`crates/warden/`, `crates/warden-gated/` et `crates/warden-tui/` (chaque crate a le sien :
ni `warden-gated` ni `warden-tui` ne dépendent de `warden`, voir ADR-0006). Toute nouvelle
requête ou migration doit régénérer le cache du crate concerné (`cargo sqlx prepare`,
exécuté depuis ce crate) et le committer avec le code — voir `code-standards.md` ("SQLite &
sqlx").

## Utiliser la CLI `warden`

Le binaire expose pour l'instant une seule sous-commande, `run`, qui exécute une boucle
de convergence complète sur un dépôt existant :

```sh
warden run \
  --repo /chemin/vers/mon-projet \
  --intent "Ajouter la validation d'email au formulaire d'inscription" \
  --tool claude
```

Aucun fichier `.md` n'est requis : `--tool claude` sélectionne l'adaptateur intégré pour
Claude Code (`warden::tool_adapter::ClaudeAdapter`), qui fournit un prompt et un jeu
d'outils par défaut pour les trois rôles (coder, reviewer, tester). Voir "Définir un agent"
ci-dessous pour reprendre la main sur un rôle en particulier via
`.warden/agents/<role>.md`.

Flags de `warden run` :

- `--repo <PATH>` — dépôt de l'utilisateur, jamais écrit directement (seuls les
  worktrees créés sous `--warden-home` le sont).
- `--intent <TEXT>` — description de la tâche transmise à l'agent coder sur son `stdin`
  (voir "Protocole d'entrée des agents (stdin)" ci-dessous, ADR-0012). Doit être non vide
  (espaces exclus) — validé dès la frontière CLI plutôt qu'en profondeur du premier cycle.
- `--tool <name>` (**requis**) — sélectionne l'adaptateur d'outil intégré qui pilote les
  trois rôles de ce run (issue #24) : l'invocation CLI réelle, l'allowlist d'environnement,
  la traduction de la sortie de l'outil en findings, et le prompt/`tools` par défaut de
  chaque rôle en l'absence de définition (voir "Définir un agent" ci-dessous). Ensemble
  fermé résolu à la compilation ; seul `claude` existe aujourd'hui. Global au run — pas de
  sélection par rôle (`--coder-tool`…, hors périmètre). Remplace les flags
  `--coder-agent`/`--reviewer-agent`/`--tester-agent` et le schéma de définition
  warden-natif qu'ils sélectionnaient (ADR-0013, amendée par l'issue #24) — voir
  `CHANGELOG.md` pour la note de migration.
- `--branch <NAME>` — nom de branche enregistré pour ce run. `warden` lui-même ne pousse
  toujours rien vers un remote (aucun credential remote côté orchestrateur, ADR-0006) ;
  c'est `warden-gated` qui reçoit un push vers son dépôt bare local et décide seul de le
  relayer vers `origin`. Défaut : `main`.
- `--max-cycles <N>` — nombre maximum de cycles coder/review/test avant abandon
  (`RunState::MaxCyclesExceeded`). Doit être ≥ 1. Défaut : `5`.
- `--warden-home <PATH>` — répertoire d'état de Warden (base SQLite + worktrees).
  Défaut : `~/.warden`.
- `--evidence-tool <playwright|asciinema>` — force l'outil de capture de preuve
  (ADR-0009) au lieu de la détection automatique du type de projet (présence d'un
  serveur/framework front → Playwright, sinon → asciinema).
- `--evidence-store-in-repo <true|false>` — commite les preuves capturées sous
  `.warden/evidence/<cycle>/` pour qu'elles apparaissent dans la PR finalisée. Activé
  par défaut (ADR-0009).
- `--gate-bare-repo <PATH>` — dépôt bare local de `warden-gated` (issue #15/ADR-0011).
  Omis, le câblage post-`Converged` entier est désactivé : un run s'arrête à `Converged`,
  exactement comme avant cette livraison.
- `--gate-gated-bin <PATH>` — chemin absolu du binaire `warden-gated` installé, requis avec
  `--gate-bare-repo` pour déclencher `run-tail`/`resume-watch` en sous-processus.
- `--gate-repo-slug <owner/repo>` — surcharge explicite du dépôt PR, au lieu de la
  résolution automatique depuis le remote `origin`.
- `--gate-poll-interval-secs <N>` (défaut `15`) et `--gate-inactivity-timeout-secs <N>`
  (défaut `1800`) — mêmes réglages que `warden-gated watch-pr` (voir "CI Watcher"
  ci-dessous), transmis tels quels au `run-tail`/`resume-watch` déclenché.
- `-v`, `-vv`, `-vvv` — verbosité des logs (`warn` par défaut, jusqu'à `trace`).

### Preuve d'exécution (Evidence)

Après chaque cycle dont le tester ne remonte aucun finding bloquant (test e2e réussi),
Warden déclenche un **Evidence Capture Adapter** dans le worktree du tester, avant sa
suppression (ADR-0009) :

- **Playwright** pour un projet web/UI (détecté via un `package.json` référençant un
  framework front, ou un marker comme `index.html`) — capture les captures
  d'écran/vidéos produites par `npx playwright test` sous `test-results/`.
- **asciinema** sinon (projet CLI) — enregistre la commande tester elle-même via
  `asciinema rec`.

Les artefacts atterrissent d'abord en stockage local (`<warden-home>/evidence/<run_id>/<cycle>/`,
jamais dans un dépôt git), puis — si `--evidence-store-in-repo` (défaut) — sont commités
sous `.warden/evidence/<cycle>/` dans un commit dédié au moment de la convergence, avant
que `Finalize` ne pousse le contenu final (jamais avant, ADR-0007). Ceci est entièrement
automatique et câblé dans la boucle de convergence de `warden` — un run qui converge avec
`--evidence-store-in-repo` (défaut) porte réellement ce commit d'évidence.

Le renderer de la section **Evidence** du corps de PR (`warden_core::format_evidence_section` :
images intégrées en inline via l'URL de contenu brut du repo, vidéos/logs/enregistrements
asciinema en lien cliquable) est lui aussi livré et appelé par `warden-gated::pr_manager::finalize`.
Mais, comme documenté dans "Gestion des PR" ci-dessous, `Finalize` lui-même n'est pas encore
déclenché automatiquement par `warden` (câblage laissé à une décision d'architecture
distincte, Phase 4) : tant que ce déclenchement n'existe pas, la section Evidence
n'apparaît donc pas encore dans une vraie PR GitHub, même si les preuves, elles, sont bien
committées dans le dépôt à chaque convergence.

Un outil de capture absent ou en échec (Playwright/asciinema non installés, aucun
artefact produit, ...) est loggé (`tracing::warn!`) et n'interrompt jamais un run par
ailleurs convergent — l'absence de preuve pour un cycle donné n'est pas traitée comme un
finding bloquant.

### Définir un agent (fichier markdown, optionnel)

Aucune définition n'est requise : `--tool claude` fournit déjà un prompt et un jeu
d'outils par défaut pour les trois rôles (voir "Flags de `warden run`" ci-dessus). Pour
reprendre la main sur un rôle en particulier, placez un fichier markdown à l'emplacement
**conventionnel** `<repo>/.warden/agents/<role>.md` (`coder.md`, `reviewer.md` ou
`tester.md`, à la racine du dépôt passé à `--repo`) — Warden le détecte et le lit
automatiquement, sans flag à passer. Le format est celui de **Claude Code**
(`.claude/agents/*.md`, issue #24) : un frontmatter **YAML** fencé par `---`, puis le
corps markdown — le **system prompt** du rôle.

```markdown
---
name: coder
description: Implémente la tâche demandée sur la branche courante.
tools: Read, Write, Edit, Bash
model: sonnet
---

Tu es le coder de Warden. Lis le payload JSON sur stdin (`intent`, `findings`),
implémente la tâche demandée, puis commite dans le worktree courant.
```

- `name`, `description` — acceptées pour compatibilité avec un fichier Claude Code
  existant (un vrai fichier `.claude/agents/*.md` les porte toujours), sans usage
  opérationnel côté Warden aujourd'hui.
- `tools` — chaîne verbatim passée à `--allowedTools` (même format qu'un `tools:`
  Claude Code, ex. `"Read, Write, Edit, Bash"`). **Omise**, la valeur par défaut de
  l'adaptateur pour ce rôle est utilisée à la place (jamais "aucun outil" — un agent sans
  aucun outil accordé en mode non-interactif ne peut rien faire et ferait converger le run
  à tort).
- `model` — alias ou nom de modèle verbatim passé à `--model` (ex. `"sonnet"`, `"opus"`).
  Omis, laisse l'outil choisir son propre modèle par défaut.
- Le corps markdown après le frontmatter est le system prompt. Il ne doit pas être vide.
- Toute clé inconnue, ou une clé optionnelle présente mais vide, est une erreur typée à la
  lecture — jamais silencieusement ignorée.

Le schéma est celui de **Claude Code**, adopté directement (issue #24, amende ADR-0013) :
Warden reste agnostique de l'agent (ADR-0005) au niveau du seam `ToolAdapter` — un nouveau
CLI supporté est un nouvel adaptateur, jamais un registre en configuration — pas au niveau
du schéma de définition. Voir `docs/Architecture.md` (ADR-0013, amendement issue #24) pour
la discussion complète, y compris pourquoi le schéma warden-natif `+++`/TOML introduit par
#22 a été abandonné (coût réel trop élevé pour lancer un vrai run).

> ⚠️ **Le fichier n'engage que le prompt/`tools`/`model` — pas l'invocation elle-même.**
> Contrairement au schéma warden-natif qu'il remplace, une définition ne déclare plus de
> `program`/`args` : c'est l'adaptateur sélectionné par `--tool` qui construit
> l'invocation réelle du CLI (`ClaudeAdapter` lance toujours `claude` via `PATH`, jamais un
> chemin relatif au worktree du rôle). Le risque « le coder committe un script que le
> reviewer exécute ensuite », réel avec l'ancien runner `command` (ADR-0013), ne s'applique
> donc plus aux adaptateurs intégrés.

### Protocole d'entrée des agents (stdin)

Chaque agent lancé par `warden` (coder, reviewer, tester) reçoit sur son `stdin` un unique
payload JSON versionné (`warden_core::AgentInputMessage`, ADR-0012/ADR-0013), puis `stdin`
est fermé (EOF) — un agent qui n'ouvre ou ne lit jamais stdin est un comportement légitime,
non fatal au run :

```json
{"version": 2, "role": "coder", "system_prompt": "Tu es le coder de Warden. ...", "intent": "Ajouter la validation d'email au formulaire d'inscription", "target_commit": null, "diff": null, "findings": []}
```

- `role` : `"coder"`, `"reviewer"` ou `"tester"` — toujours présent.
- `system_prompt` : le corps markdown de la définition du rôle (ADR-0013) — toujours présent,
  jamais vide. Ce champ stdin reste le canal warden-géré pour ce texte (jamais un fichier de
  prompt temporaire). **Exception ciblée (issue #24, `ClaudeAdapter`)** : `claude` n'a pas
  d'autre canal qu'un argument pour recevoir un system prompt (son stdin, en mode texte,
  *est* le tour utilisateur) — l'adaptateur passe donc la même chaîne une seconde fois en
  argument (`--append-system-prompt`), un compromis assumé et documenté plutôt qu'une fuite
  accidentelle (`warden::tool_adapter::ClaudeAdapter`, `docs/Architecture.md` ADR-0013).
- Coder : `intent` (la tâche du run, cf. `--intent`) et `findings` (ceux qui ont déclenché ce
  cycle — ce qu'il doit corriger ; liste vide sur le premier cycle d'un run).
  `target_commit`/`diff` valent `null` : le worktree du coder est déjà checkouté sur le commit
  concerné, il fait son `git diff` lui-même.
- Reviewer/tester : `target_commit` (le commit produit par le coder de ce cycle), `diff`
  (`git diff` entre le début et la fin du cycle — peut être vide si le coder n'a rien
  committé) et `findings` (ceux qui ont déclenché ce cycle, y compris les findings CI sur un
  reboucle post-convergence ; liste vide sur le premier cycle d'un run) ; `intent` vaut
  `null`. Le `diff` est tronqué à 8 Mio ; un diff tronqué porte le marqueur
  `\n\n[warden: diff truncated at the 8 MiB payload cap]\n` en fin de champ, détectable côté
  agent plutôt que silencieusement coupé.
- L'environnement du sous-processus reste construit par `env_clear()` (jamais un héritage
  brut) : par défaut seul `PATH` est transmis, plus l'allowlist explicite que l'adaptateur
  `--tool` sélectionné déclare (`HOME`/`USER` pour `claude`, voir "Sécurité" dans
  `docs/Architecture.md`, §10 et ADR-0006 amendée). Ce payload stdin reste le seul canal par
  lequel l'intent/le contexte du run (findings, diff, ...) atteint l'agent — jamais une
  variable d'environnement ni un argument de ligne de commande, contrairement au system
  prompt lui-même qui, pour `claude`, voyage en argument (`--append-system-prompt`, voir
  "Définir un agent" ci-dessus).

### Protocole de sortie des agents (findings)

Les agents `reviewer` et `tester` doivent écrire sur `stdout` un flux **NDJSON**
(une valeur JSON par ligne, pas de tableau/objet englobant). Chaque ligne représente un
finding :

```json
{"source": "reviewer", "severity": "blocking", "file": "src/lib.rs", "description": "unwrap non géré", "action": "utiliser ? à la place"}
```

- `source` : `"reviewer"` ou `"tester"`.
- `severity` : `"blocking"`, `"warning"` ou `"info"`. Un finding `blocking` déclenche un
  nouveau cycle (ou `MaxCyclesExceeded` si le budget est épuisé) ; sans finding
  `blocking`, le run passe à `Converged`.
- `file` et `action` sont optionnels ; `description` est requis.

Toute ligne non vide qui n'est pas un JSON valide, ou dont `severity`/`source` sort de
cet ensemble fermé, fait échouer le parsing (`warden_core::parse_findings`) — jamais de
confiance aveugle dans la sortie d'un agent, cf. `code-standards.md`.

## `warden-tui` (moniteur en lecture seule)

`warden-tui` est un binaire séparé (ADR-0008) qui permet de suivre un run en direct depuis
un terminal différent de celui qui l'a lancé, **strictement en lecture seule** : aucune
commande d'action (approve/fix/skip) ne transite par lui, et il n'écrit jamais dans la
SQLite de `warden`, ne spawn aucun agent, ne touche jamais git.

```sh
warden-tui attach --run-id <RUN_ID> --warden-home ~/.warden
```

- `--run-id <ID>` — l'identifiant de run. Affiché par `warden run` dès le démarrage
  (`run <id> started`, suivi d'une commande `warden-tui attach` prête à copier), et de
  nouveau à la fin de son exécution (aussi consultable en base, table `runs`).
- `--db <PATH>` — base SQLite de `warden`, ouverte en lecture seule. Défaut :
  `<warden-home>/state.db`.
- `--warden-home <PATH>` — sert à localiser la base et le socket de l'Event Bus du run.
  Défaut : `~/.warden`.

**Event Bus + replay** : `warden` publie chaque événement significatif (démarrage de run,
de cycle, d'agent, finding remonté, fin de run) à la fois sur un socket Unix local
(`~/.warden/runs/<run_id>.sock`, permissions `0600`, strictement lecture seule — le module
qui l'implémente ne lit jamais les octets écrits par un abonné) et dans la table `events`
en SQLite. `warden-tui` s'abonne d'abord au socket, puis relit l'historique complet en
base, avant de fusionner les deux (déduplication par identifiant d'événement) : une
attache tardive sur un run déjà en cours affiche donc l'historique complet puis bascule
sur le direct, sans trou.

**Progression d'agent en direct (issue #33, amende ADR-0008)** : entre `AgentStarted` et
`AgentFinished`, `warden-tui` affiche désormais ce que l'agent rapporte faire — dernier
message assistant complet ou bloc `tool_use` — au fur et à mesure (`RunEvent::AgentProgress`),
au lieu de rester sans nouvelle jusqu'à la fin de l'agent. `ClaudeAdapter` (`--tool claude`)
lance `claude` avec `--output-format stream-json --verbose` pour l'obtenir. Ce signal est
**live-only** : il transite sur l'Event Bus mais n'est **jamais persisté** en base (contrairement
aux autres événements de cette section) — une attache tardive ne rejoue donc jamais la
progression d'un agent déjà en cours ou déjà terminé, elle attend le prochain événement en
direct. C'est aussi une observation **déclarative** (ce que l'agent affirme faire), pas une
preuve vérifiée d'exécution — voir "Preuve d'exécution (Evidence)" ci-dessus (ADR-0009) pour
la seule source qui porte une valeur de preuve.

Sur un terminal dont la sortie standard n'est pas un TTY (pipe, redirection), `warden-tui`
bascule automatiquement sur un mode texte qui affiche un événement par ligne en NDJSON —
pratique pour scripter/observer un run sans interface plein écran.

**Rendu de l'evidence (ADR-0010)** : au démarrage (terminal plein écran uniquement),
`warden-tui` détecte les capacités graphiques du terminal (Kitty, iTerm2, Sixel, via
`ratatui-image`) et affiche les images capturées inline lorsque c'est possible, avec
fallback sur un visualiseur externe sinon. **Ce qui n'est pas encore câblé** : la Phase 7
(Evidence Capture Adapter, issue #7) qui produirait réellement ces captures (table
`EVIDENCE`) n'est pas encore livrée sur cette branche — le rendu inline d'image est
fonctionnel et testé (`crates/warden-tui/src/evidence.rs`), mais l'extraction de frame
vidéo (`ffmpeg`) et la lecture asciinema en sous-terminal sont pour l'instant des erreurs
typées explicites (`TuiError::NotYetImplemented`), en attendant qu'une source de données
réelle existe pour les exercer.

## Le gate git (`warden-gated`)

`warden-gated` est un binaire séparé qui ne partage aucun code d'accès à la base avec
`warden` (voir `crates/warden-gated/src/db.rs` — requêtes dupliquées volontairement,
ADR-0006) : il ouvre la SQLite de `warden` en connexion strictement **lecture seule**, et
relit lui-même l'état persisté d'un run (`RunState`, hash de convergence) avant d'autoriser
le moindre push vers `origin`. Il ne fait jamais confiance à une notification ou à ce que
`warden` prétend.

### Mise en place

```sh
# Crée (si besoin) le dépôt bare local et y installe le hook post-receive minimal.
warden-gated init-bare \
  --bare-repo ~/.warden/gate.git \
  --bin "$(which warden-gated)" \
  --socket ~/.warden/gated.sock \
  --origin-url git@github.com:mon-org/mon-repo.git

# Démarre le daemon (à lancer via un service managé en usage réel, cf. `contrib/`).
warden-gated serve \
  --socket ~/.warden/gated.sock \
  --db ~/.warden/state.db \
  --bare-repo ~/.warden/gate.git \
  --branch main
```

Un push vers le dépôt bare sur `refs/heads/warden-run/<run_id>` déclenche le hook
`post-receive` installé (relais minimal, aucune logique métier — voir `hook.rs`), qui
transmet la notification brute au daemon via un socket Unix. Le daemon relit alors l'état
réel du run en base et ne relaie le push vers `origin` que si `state == Converged` **et**
le commit poussé correspond au hash validé (`runs.converged_commit_sha`) ; sinon, le push
est bloqué et loggé, sans jamais toucher `origin`.

Le socket d'écoute (`--socket`) est automatiquement restreint en lecture/écriture au seul
propriétaire (`0600`) dès son ouverture par `serve` — le répertoire qui le contient
(`~/.warden` par défaut) doit néanmoins rester lui-même privé à cet utilisateur pour que
cette restriction ait un sens.

**Ce push initial vers `refs/heads/warden-run/<run_id>` n'est aujourd'hui déclenché par
personne automatiquement** : câbler `warden` pour qu'il le fasse à la convergence est du
ressort de la Phase 4 (non couverte par ce dépôt pour l'instant). Le mécanisme ci-dessus
(hook, socket, revérification, push vers `origin`) est complet et testé de bout en bout
(`crates/warden-gated/tests/cli.rs`) via un `git push` manuel/scripté vers le dépôt bare ;
seul le déclenchement automatique côté orchestrateur reste à écrire.

`warden-gated verify-run --db <path> --run-id <id> --commit <sha>` est un utilitaire de
diagnostic qui rejoue cette même revérification indépendamment de tout push réel (code de
sortie `0` = autorisé, `1` = bloqué).

### Gestion des PR (`OpenDraft` / `PostCycleUpdate` / `Finalize`)

`warden-gated` expose aussi, comme capacité de bibliothèque (`crates/warden-gated/src/pr_manager.rs`),
trois actions formant le cycle de vie d'une PR, adossées à un trait `PrProvider` implémenté
aujourd'hui via la CLI `gh` (`gh_provider::GhProvider`) — celle-ci réutilise la session `gh`
déjà authentifiée de la machine : `warden-gated` ne stocke ni ne lit lui-même de credential
GitHub.

- `OpenDraft` — pousse un commit squelette de branche **sans aucun contenu métier**
  (vérifié indépendamment avant tout push : comparaison d'arbre net contre la branche de
  base, et parcours commit par commit de l'historique effectivement transféré, y compris le
  contenu propre d'un commit de fusion) et ouvre une PR en draft, liée à l'issue détectée
  dans l'intent (`(?i)(fixes|closes|resolves)\s+#\d+`) ou titrée à partir de l'intent sinon.
- `PostCycleUpdate` — poste un commentaire informatif par cycle (findings reviewer/tester)
  sur la PR ; ne modifie jamais son statut draft ni son contenu.
- `Finalize` — revérifie `state == Converged` et le hash committé via le même chemin
  `gate::verify_and_authorize` que le gate git lui-même, puis seulement si autorisé : pousse
  le contenu réel, met à jour le corps de la PR et retire le statut draft.

Ce module fournit également le formatage des attributs de commit structurés
(`Warden-Cycle`, `Warden-Findings-Resolved`, `Warden-Agent`) destinés aux commits coder/doc.

**Câblage (issue #15/ADR-0011)** : `OpenDraft` et `Finalize` sont désormais invoquées
automatiquement par `warden-gated run-tail`/`resume-watch`, que `warden` déclenche en
sous-processus une fois un run convergé poussé dans le dépôt bare local (voir "Le câblage
`run-tail`/`resume-watch`" ci-dessous) — plus besoin d'un déclenchement manuel.
`PostCycleUpdate`, elle, reste pour l'instant une capacité de bibliothèque non invoquée
automatiquement par `warden` (hors périmètre de cette livraison).

### CI Watcher (`watch-pr`)

`warden-gated` surveille une PR déjà ouverte jusqu'à un statut terminal, via le trait
`CiProvider` (`crates/warden-gated/src/ci_watcher.rs`) — implémenté aujourd'hui par
`gh_provider::GhProvider`, qui réutilise la même session `gh` authentifiée que le PR Manager
(`gh pr view --json state,statusCheckRollup`, sans jamais stocker de credential GitHub).

```sh
warden-gated watch-pr \
  --bare-repo ~/.warden/gate.git \
  --pr 42 \
  --poll-interval-secs 15 \
  --inactivity-timeout-secs 1800
```

- `--bare-repo <PATH>` — sert à résoudre `owner/repo` depuis le remote `origin` du dépôt
  bare, comme `GhProvider::new` le fait déjà pour `OpenDraft`/`Finalize`. `--repo
  <owner/repo>` permet de court-circuiter cette résolution.
- `--poll-interval-secs` (défaut `15`) — délai entre deux interrogations ; la boucle ne fait
  jamais de busy-spin, elle attend systématiquement ce délai (`tokio::time::sleep`) entre deux
  appels.
- `--inactivity-timeout-secs` (défaut `1800`) — durée maximale pendant laquelle le statut
  observé peut rester **strictement inchangé** avant abandon (`TimedOut`) ; jamais d'attente
  infinie. Cette horloge se réinitialise à chaque changement de statut réellement observé
  (nouveau check, check en cours qui se termine) — une CI qui progresse encore n'est jamais
  interrompue prématurément, seul un statut resté figé pendant tout ce délai déclenche le
  timeout.

Un échec transitoire de poll (`gh` injoignable, rate limit réseau) est toléré et retenté
jusqu'à 3 fois consécutives (compteur réinitialisé dès le prochain poll réussi) avant que
`watch-pr` n'abandonne ; une réponse malformée ou inattendue de `gh`, elle, fait toujours
échouer `watch-pr` immédiatement, sans retry.

Statuts terminaux et code de sortie (même convention que `verify-run`) :

- `MERGED` / `CHECKS-PASSED` (exit `0`) — dans les deux cas, la décision de merger reste
  **entièrement humaine** : `CiProvider`/`ci_watcher` n'exposent aucune capacité de merge,
  Warden ne merge jamais automatiquement une PR.
- `CLOSED` (fermée sans merge), `CHECKS-FAILED` (un finding bloquant par check en échec,
  `FindingSource::Ci`), `TIMED-OUT` — exit non nul.

Validé de bout en bout sur de vrais dépôts GitHub publics : une PR déjà mergée (`MERGED`),
une PR ouverte dont tous les checks sont verts (`CHECKS-PASSED`), et une PR sans aucune CI
configurée (`TIMED-OUT` déclenché proprement au bout du délai configuré, sans busy-spin).

La fonction pure `warden_core::decide_next_state_after_ci` décide le `RunState`
(`Done` / `CoderRunning` / `Failed`) qu'implique un résultat de watch — miroir de
`decide_next_state` pour les findings reviewer/tester. **Câblage (issue #15/ADR-0011)** :
cette décision est désormais appliquée automatiquement par `warden` lui-même — voir
"Le câblage `run-tail`/`resume-watch`" ci-dessous.

### Le câblage `run-tail`/`resume-watch` (issue #15/ADR-0011)

`warden` ne lance plus jamais `watch-pr` en diagnostic pour driver un run réel : ce sont les
deux sous-commandes `warden-gated run-tail` et `warden-gated resume-watch` qui encapsulent
la suite `OpenDraft`/`Finalize` (ou juste `Finalize` si une PR existe déjà pour ce run) puis
`watch_pr`, et qui livrent le résultat terminal à `warden` par le socket Unix inverse décrit
plus haut :

- `warden-gated run-tail` — chemin nominal, déclenché par `warden` juste après avoir poussé
  le commit convergé dans le dépôt bare local. Ouvre une nouvelle PR (`OpenDraft`) sauf si le
  run en a déjà une (`runs.pr_number`, cas d'un reboucle sur `ChecksFailed`), auquel cas il
  saute directement à `Finalize` plutôt que de rouvrir une seconde PR draft.
- `warden-gated resume-watch` — contrepartie de reprise après crash : pour un run retrouvé
  bloqué en `AwaitingCi` au redémarrage de `warden` (la PR est déjà ouverte/finalisée), ne
  fait que reprendre `watch_pr` sur la PR existante et en délivrer le résultat.

Les deux sous-commandes revérifient indépendamment l'état du run depuis leur propre lecture
seule de la SQLite avant d'agir (jamais confiance aveugle envers ce que `warden` prétend,
ADR-0006) et acceptent les mêmes options `--poll-interval-secs`/`--inactivity-timeout-secs`
que `watch-pr` ci-dessus. L'attente de leur résultat côté `warden` est bornée par la durée de
vie du sous-processus déclenché (pas un timeout mur-à-mur indépendant côté `warden`) : tant
que le sous-processus est vivant, `warden` continue d'attendre son message terminal ; s'il
sort sans en avoir délivré un, le run est marqué `Failed`.

### Service managé

`warden-gated` est prévu pour tourner comme service managé (survit à un redémarrage
machine) :

- Linux (systemd, service utilisateur) : `crates/warden-gated/contrib/systemd/warden-gated.service`.
- macOS (launchd, agent utilisateur) : `crates/warden-gated/contrib/launchd/com.warden.gated.plist`.

Les deux fichiers documentent leur installation en commentaire, sous la forme la plus
simple (même utilisateur OS que `warden`) — voir la limite d'isolation qui en découle
ci-dessous et dans "Déploiement durci". Les credentials du remote réel (clé SSH,
credential helper) doivent être utilisables par ce service au démarrage — `warden-gated`
ne les embarque jamais lui-même.

### Déploiement durci (isolation OS réelle)

Le déploiement documenté ci-dessus (unités `contrib/`, même utilisateur OS pour `warden`
et `warden-gated`) donne une frontière **process/logique**, pas une isolation **OS** : les
deux binaires ne partagent aucun code d'accès aux credentials et `warden-gated` revérifie
tout de manière indépendante, mais un `warden` compromis au niveau code, tournant sous le
même UID, peut lire directement ce que cet UID peut lire — y compris la clé SSH ou le
credential helper d'`origin`. C'est un choix documenté, pas un oubli (voir ADR-0006,
section "Précision v1" dans `docs/Architecture.md`).

Pour une isolation qui tient même si `warden` est compromis **au niveau code** :

1. Créer un utilisateur OS dédié (ex. `warden-gate`), distinct de celui qui exécute
   `warden`.
2. Faire posséder à cet utilisateur, et à lui seul, le dépôt bare local
   (`~/.warden/gate.git` sous son propre `$HOME`) et les credentials `origin` (clé SSH
   privée ou credential helper configuré pour son compte uniquement — permissions fichier
   excluant l'utilisateur qui exécute `warden`).
3. Lancer `warden-gated serve` comme service managé sous cet utilisateur dédié (adapter
   les chemins `%h`/`HOME_PLACEHOLDER` des fichiers `contrib/` en conséquence).
4. Donner à l'utilisateur de `warden` uniquement un accès en écriture au dépôt bare (ex.
   via un push SSH vers `warden-gate@localhost:gate.git`, ou un partage de groupe Unix
   limité à ce seul répertoire) — jamais un accès aux credentials `origin` eux-mêmes.

Ce mode n'est pas automatisé par les fichiers `contrib/` fournis (qui visent la simplicité
d'installation mono-utilisateur) ; il nécessite une configuration système manuelle
correspondant à l'infrastructure de déploiement réelle.

## Documentation

Le dossier d'architecture est maintenu dans un vault Obsidian, hors dépôt. En local,
`docs/` est un lien symbolique vers ce dossier (non versionné, cf. `.gitignore`).

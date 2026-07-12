# Warden

Orchestrateur local, écrit en Rust, qui pilote un cycle de développement assisté par
plusieurs agents IA spécialisés (coder, reviewer, tester, rédacteur de doc) jusqu'à
convergence, puis livre le résultat via un point de passage git indépendant du jugement
des agents.

## État du projet

Phase 1 (fondations), Phase 2 (parallélisme réel) et Phase 3 (gate git) sont livrées :
un workspace Cargo avec le binaire `warden` (orchestrateur), capable de piloter une boucle
de convergence (coder → review/test → reboucle si besoin) avec persistance SQLite et
reprise après crash : au redémarrage, tout run laissé dans un état intermédiaire sans
processus agent vivant est marqué `Failed`, et les ressources qu'il a pu laisser
orphelines (worktrees git, processus agents encore en vie) sont automatiquement
récupérées — y compris si un second crash interrompt la récupération elle-même. Une
sauvegarde de la base SQLite est également prise avant toute migration de schéma.
Reviewer et tester tournent **en parallèle** (`tokio::join!`), chacun
dans son propre worktree synchronisé sur le commit du coder. Un second binaire,
`warden-gated`, forme désormais la frontière de sécurité vers le remote réel
(ADR-0002/ADR-0006) : il ne partage aucun code I/O avec `warden`, relit lui-même l'état du
run et le hash validé en SQLite (connexion strictement lecture seule) avant tout push vers
`origin`, et ne fait jamais confiance à ce que `warden` prétend. La TUI (`warden-tui`)
arrive dans une phase ultérieure et n'existe pas encore.

**Ce qui n'est pas encore câblé (Phase 4)** : `warden` lui-même ne pousse pas encore
automatiquement les runs convergés vers le dépôt bare de `warden-gated`, et la transition
`Converged` → `Pushed` de la state machine n'est pas encore déclenchée par l'orchestrateur.
`warden-gated` (hook, socket, revérification, push vers `origin`) est entièrement
fonctionnel et testé de bout en bout indépendamment, mais le déclenchement du premier push
(`git push` vers le dépôt bare local à la convergence) reste, pour l'instant, une étape
manuelle ou scriptée — voir "Le gate git" ci-dessous.

Le **PR Manager** (`OpenDraft` / `PostCycleUpdate` / `Finalize`) a lui aussi livré côté
bibliothèque dans `warden-gated` — voir "Gestion des PR" ci-dessous — mais, de la même
manière, aucun déclenchement CLI/IPC ne l'invoque encore depuis `warden` : ce câblage reste
une décision d'architecture distincte, non couverte par cette livraison.

Le **CI Watcher** (Phase 5) est livré et utilisable de bout en bout via la sous-commande
`warden-gated watch-pr` — voir "CI Watcher" ci-dessous. Le câblage de sa décision
(reboucler vers le coder ou non) dans la boucle de convergence de `warden` reste, comme
pour le PR Manager, une décision d'architecture distincte hors périmètre de cette livraison.
Warden ne merge **jamais** automatiquement une PR, quel que soit le statut observé.

L'**Evidence Capture Adapter** (Phase 7, ADR-0009) est livré et entièrement câblé dans la
boucle de convergence de `warden` : chaque cycle dont le tester réussit son test e2e produit
une preuve (Playwright ou asciinema selon le projet), committée dans le dépôt à la
convergence si `--evidence-store-in-repo` (défaut) — voir "Preuve d'exécution (Evidence)"
ci-dessous. Le renderer de la section Evidence du corps de PR est lui aussi livré et appelé
par `warden-gated::pr_manager::finalize`, mais son affichage dans une vraie PR GitHub dépend
du câblage `Finalize` du PR Manager décrit au paragraphe précédent, qui n'existe pas encore.

**Limite d'isolation à connaître avant tout déploiement** : dans la configuration par
défaut documentée ici, `warden` et `warden-gated` tournent sous le **même utilisateur OS**.
Cela donne une frontière de sécurité **process/logique** (aucun code d'accès credentials
partagé, revérification indépendante en base) mais **pas** une isolation OS — un `warden`
compromis au niveau code, sous cet UID, peut lire directement les credentials `origin`. Voir
"Déploiement durci" ci-dessous et ADR-0006 dans `docs/Architecture.md` pour la configuration
qui ferme cet écart.

## Structure du dépôt

- `crates/warden-core/` — logique pure (state machine des runs, interprétation des
  findings), 100 % testable sans I/O.
- `crates/warden/` — binaire orchestrateur (`[[bin]] warden`) : CLI, gestion des
  worktrees git, spawn des agents en sous-processus, persistance SQLite (`sqlx`),
  boucle de convergence.
- `crates/warden-gated/` — binaire du gate git (`[[bin]] warden-gated`) : seul détenteur
  des credentials vers `origin`, hook `post-receive` minimal + revérification indépendante
  de l'état avant tout push (voir "Le gate git (`warden-gated`)" ci-dessous).

## Compilation & tests

```sh
cargo build
cargo test
```

Ces commandes fonctionnent **hors ligne**, sans base de données ni `DATABASE_URL` : les
requêtes `sqlx` sont vérifiées à la compilation via les caches `.sqlx/` committés dans
`crates/warden/` et `crates/warden-gated/` (chaque crate a le sien : `warden-gated` ne
dépend pas de `warden`, voir ADR-0006). Toute nouvelle requête ou migration doit
régénérer le cache du crate concerné (`cargo sqlx prepare`, exécuté depuis ce crate) et le
committer avec le code — voir `code-standards.md` ("SQLite & sqlx").

## Utiliser la CLI `warden`

Le binaire expose pour l'instant une seule sous-commande, `run`, qui exécute une boucle
de convergence complète sur un dépôt existant :

```sh
warden run \
  --repo /chemin/vers/mon-projet \
  --intent "Ajouter la validation d'email au formulaire d'inscription" \
  --coder-cmd "claude -p coder.md" \
  --reviewer-cmd "claude -p reviewer.md" \
  --tester-cmd "claude -p tester.md"
```

Flags de `warden run` :

- `--repo <PATH>` — dépôt de l'utilisateur, jamais écrit directement (seuls les
  worktrees créés sous `--warden-home` le sont).
- `--intent <TEXT>` — description de la tâche transmise à l'agent coder.
- `--coder-cmd <CMD>`, `--reviewer-cmd <CMD>`, `--tester-cmd <CMD>` — commandes shell
  (programme + arguments, découpées sur les espaces) lançant respectivement l'agent
  coder, reviewer et tester en sous-processus.
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

**Ce qui n'est pas encore câblé** : ces trois actions existent uniquement comme capacité de
bibliothèque — aucun déclenchement CLI/IPC ne les invoque encore depuis `warden`. Ce câblage
est une décision d'architecture distincte, hors périmètre de cette livraison.

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
`decide_next_state` pour les findings reviewer/tester. **Ce qui n'est pas encore câblé** :
`warden-gated` ne fait que remonter le résultat de `watch-pr` ; brancher cette décision dans
la boucle de convergence de `warden` (pour reboucler automatiquement vers le coder sur un
`ChecksFailed`) est, comme pour le PR Manager, une décision d'architecture distincte hors
périmètre de cette livraison.

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

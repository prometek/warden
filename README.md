# Warden

Orchestrateur local, écrit en Rust, qui pilote un cycle de développement assisté par
plusieurs agents IA spécialisés (coder, reviewer, tester, rédacteur de doc) jusqu'à
convergence, puis livre le résultat via un point de passage git indépendant du jugement
des agents.

## État du projet

Phase 1 (fondations), Phase 2 (parallélisme réel) et Phase 3 (gate git) sont livrées :
un workspace Cargo avec le binaire `warden` (orchestrateur), capable de piloter une boucle
de convergence (coder → review/test → reboucle si besoin) avec persistance SQLite et
reprise après crash. Reviewer et tester tournent **en parallèle** (`tokio::join!`), chacun
dans son propre worktree synchronisé sur le commit du coder. Un second binaire,
`warden-gated`, forme désormais la frontière de sécurité vers le remote réel
(ADR-0002/ADR-0006) : il ne partage aucun code I/O avec `warden`, relit lui-même l'état du
run et le hash validé en SQLite (connexion strictement lecture seule) avant tout push vers
`origin`, et ne fait jamais confiance à ce que `warden` prétend. La TUI (`warden-tui`)
arrive dans une phase ultérieure et n'existe pas encore.

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
- `-v`, `-vv`, `-vvv` — verbosité des logs (`warn` par défaut, jusqu'à `trace`).

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

`warden-gated verify-run --db <path> --run-id <id> --commit <sha>` est un utilitaire de
diagnostic qui rejoue cette même revérification indépendamment de tout push réel (code de
sortie `0` = autorisé, `1` = bloqué).

### Service managé

`warden-gated` est prévu pour tourner comme service managé (survit à un redémarrage
machine) :

- Linux (systemd, service utilisateur) : `crates/warden-gated/contrib/systemd/warden-gated.service`.
- macOS (launchd, agent utilisateur) : `crates/warden-gated/contrib/launchd/com.warden.gated.plist`.

Les deux fichiers documentent leur installation en commentaire. Les credentials du remote
réel (clé SSH, credential helper) doivent être utilisables par ce service au démarrage —
`warden-gated` ne les embarque jamais lui-même.

## Documentation

Le dossier d'architecture est maintenu dans un vault Obsidian, hors dépôt. En local,
`docs/` est un lien symbolique vers ce dossier (non versionné, cf. `.gitignore`).

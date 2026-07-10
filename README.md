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

**Ce qui n'est pas encore câblé (Phase 4)** : `warden` lui-même ne pousse pas encore
automatiquement les runs convergés vers le dépôt bare de `warden-gated`, et la transition
`Converged` → `Pushed` de la state machine n'est pas encore déclenchée par l'orchestrateur.
`warden-gated` (hook, socket, revérification, push vers `origin`) est entièrement
fonctionnel et testé de bout en bout indépendamment, mais le déclenchement du premier push
(`git push` vers le dépôt bare local à la convergence) reste, pour l'instant, une étape
manuelle ou scriptée — voir "Le gate git" ci-dessous.

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

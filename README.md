# Warden

Orchestrateur local, écrit en Rust, qui pilote un cycle de développement assisté par
plusieurs agents IA spécialisés (coder, reviewer, tester, rédacteur de doc) jusqu'à
convergence, puis livre le résultat via un point de passage git indépendant du jugement
des agents.

## État du projet

Phase 1 (fondations) et Phase 2 (parallélisme réel) sont livrées : un workspace Cargo avec
le binaire `warden`, capable de piloter une boucle de convergence (coder → review/test →
reboucle si besoin) avec persistance SQLite et reprise après crash. Reviewer et tester
tournent désormais **en parallèle** (`tokio::join!`), chacun dans son propre worktree
synchronisé sur le commit du coder. Le point de passage git (`warden-gated`) et la TUI
(`warden-tui`) arrivent dans des phases ultérieures et n'existent pas encore.

## Structure du dépôt

- `crates/warden-core/` — logique pure (state machine des runs, interprétation des
  findings), 100 % testable sans I/O.
- `crates/warden/` — binaire orchestrateur (`[[bin]] warden`) : CLI, gestion des
  worktrees git, spawn des agents en sous-processus, persistance SQLite (`sqlx`),
  boucle de convergence.

## Compilation & tests

```sh
cargo build
cargo test
```

Ces commandes fonctionnent **hors ligne**, sans base de données ni `DATABASE_URL` : les
requêtes `sqlx` sont vérifiées à la compilation via le cache `.sqlx/` committé dans
`crates/warden/`. Toute nouvelle requête ou migration doit régénérer ce cache
(`cargo sqlx prepare`) et le committer avec le code — voir `code-standards.md`
("SQLite & sqlx").

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
- `--branch <NAME>` — nom de branche enregistré pour ce run (informationnel en Phase 1 ;
  aucun push n'a lieu tant que le point de passage git n'existe pas). Défaut : `main`.
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

## Documentation

Le dossier d'architecture est maintenu dans un vault Obsidian, hors dépôt. En local,
`docs/` est un lien symbolique vers ce dossier (non versionné, cf. `.gitignore`).

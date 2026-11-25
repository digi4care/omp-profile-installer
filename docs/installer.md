# Installer guide: how `omp-topology` works

This guide explains the moving parts behind `omp-topology install` and the
commands around it, so the behavior — including its fail-closed checks —
is predictable before you run it.

## The five things an install touches

- **Topology** — a versioned, self-contained overlay in a source tree
  (`topologies/<name>/` with a `topology.yml` manifest and an `agent/`
  payload). It describes *what* gets installed.
- **OMP profile** — a named OMP configuration (e.g. `digital-product-studio`).
  OMP itself selects a profile; `omp --profile <name>` starts OMP with that
  profile.
- **Agent directory** — the on-disk directory OMP resolves for the profile
  (via `omp --profile <name> config path`), where the overlay's files and
  `config.yml` land.
- **Registry alias** — a short lowercase name (`[a-z0-9][a-z0-9-]{0,63}`)
  under which the installer records one install: target, topology, source,
  and ownership manifest. It is an **internal installer identifier only**.
  Passing `--alias` does **not** create an executable, a launcher, a shell
  alias, or any other command — nothing new appears on your `PATH`.
- **Shell command** — an executable file on your `PATH` (e.g.
  `~/.local/bin/teddy`). The installer never creates one; it only *checks*
  for one at install time, because a registry alias that shadows an existing
  command name is confusing (see [Alias name preflight](#alias-name-preflight)).

The default alias is the profile name (lowercased), or the lowercased
basename of the agent directory when there is no profile. Pass `--alias`
explicitly to choose a different name.

## Commands at a glance

| Command | What it does |
|---|---|
| `list` | Shows every installable topology in a source, with `--json` for machine output. Changes nothing. |
| `install` | Plans, then applies a topology into a target in one transaction and registers the alias. |
| `status --alias <name>` | Read-only: what the registry records, manifest health, and whether the source has moved on. |
| `update --alias <name>` | Re-applies an *existing* alias's recorded install against the current source pin; a no-op when already current. Never creates an install. |
| `remove --alias <name>` | Restores prior config values and deletes what the topology added under that alias. Files you edited since install are preserved and reported. |

Every command accepts `--json` (exactly one JSON document on stdout) and
failures print `error: <message>` on stderr with exit code 1.

**Dry run.** `install`, `remove`, `update`, and `self-update` accept
`--dry-run`: the plan is printed and no topology, registry, or persistent
source-cache writes happen. One caveat: resolving a named profile invokes
`omp --profile <name> config path`, which can create a missing profile
directory as an OMP side effect (the CLI removes it again only if empty,
best-effort). Everything else — including the alias preflight below — runs the same
checks as a real install, evaluated against the state at the time the
command runs; a later real run re-checks against whatever state it finds
then.

## Sources, refs, and versions

- `--source` selects the topology source: `github:OWNER/REPO`, an HTTPS
  GitHub URL, or a local path containing `topologies/`. A binary installed
  from a release has no checkout, so pass `--source` explicitly; running
  from a checkout defaults to the checkout root.
- `--ref` picks *which Git ref of the source tree* is read — a branch, tag,
  or commit. It only selects the checkout; it never implies a topology
  version.
- `--version` on `install`/`update` is an **exact SemVer assertion** against
  the selected source's `topology.yml` `version`: the command fails unless
  the source declares exactly that version. It is not a tag resolver.
- `update` refuses to move an install to an older topology version, so a
  stale `--ref` or source cannot silently roll you back.

## Profiles, aliases, and shared agent directories

Each alias maps to exactly one target. Re-installing under the same alias
for the same target is an upsert; reusing an alias for a different topology
or target is refused before anything is mutated.

Several aliases may target the **same agent directory** — for example one
profile extended by two independent topologies. This works only when the
installs are independent: each alias owns only the files and config keys it
installed, tracked in its own ownership manifest. Any overlap with another
alias's files or config keys is refused before any mutation — no alias
overwrites, deletes, or depends on another alias's files.

## Alias name preflight

Before an install **creates a new registry alias** — and before a
`--dry-run` that would create one prints its plan — the installer checks
that the alias name does not collide with something you already have:

1. **Registry collision.** If the alias is already registered for a
   different topology or target, the install stops and the error points you
   at the existing entry: update the existing alias with
   `omp-topology update --alias <name>`, or choose another `--alias`.
2. **Executable collision.** If an **executable file** named exactly like
   the alias exists on your `PATH`, the install stops before touching
   anything. The error names the alias and the colliding executable path
   and suggests choosing another `--alias`.

An install under an **already-registered** alias for the same target and
topology is an intentional upsert, not a collision: it is not re-checked
against the `PATH` and proceeds normally. `update` remains the normal
workflow for refreshing an existing install, and `remove` and `status` are
unchanged by this preflight — all three only ever touch an alias the
registry already holds.

Scope of the executable check:

- It matches **executable files on `PATH` only**. A non-executable file or
  a directory with the same name does not block an install.
- **Shell aliases and shell functions cannot be detected** — they live in
  your shell session, not on `PATH` — and are out of scope. If you defined
  `alias teddy=...` in your shell, the installer cannot see it; pick a
  distinct alias anyway.

### Worked example: an existing `teddy` command

Say you have an executable at `/home/digi4care/.local/bin/teddy` (some
unrelated app), and you want the `digital-product-studio` topology in an
OMP profile named `teddy`:

```sh
# Blocked: the alias 'teddy' collides with the executable
# /home/digi4care/.local/bin/teddy — the installer stops before any
# registry or target change and suggests another alias.
omp-topology install -t digital-product-studio \
  --profile teddy --alias teddy \
  --source github:digi4care/omp-topologies

# Works: a distinct alias with no PATH collision.
omp-topology install -t digital-product-studio \
  --profile teddy --alias omp-teddy \
  --source github:digi4care/omp-topologies
```

The alias is only the installer's registry key — it does not shadow or
replace the external `teddy` app. You still run OMP with
`omp --profile teddy`; that selects the OMP **profile** and does not run
the external `teddy` executable, and `omp-teddy` never becomes a command.

## File ownership collisions

Independent of the alias preflight, the install transaction itself refuses
to overwrite anything it does not own: existing files that differ from the
topology's bytes, and config keys already owned by another alias. These
checks run before any mutation, so a refused install leaves the target
untouched. To resolve one, remove the owning alias's install first
(`remove --alias <name>`) or adjust the topology.

## Troubleshooting: `unknown alias`

`update`, `remove`, and `status` operate on aliases already in the registry.
`unknown alias '<name>'` means no install is registered under that exact
name — aliases are case-sensitive lowercase slugs, and `update` never
creates one. Inspect the registered aliases with
`omp-topology profiles list`, which shows the discovered profiles and the
managed installs under each, and mind the difference between the alias
(installer identifier) and the profile name (what `omp --profile` uses):
they default to the same value but diverge as soon as you pass `--alias`.

There is no `rename` subcommand: the registered alias remains the lifecycle
identifier for `update`, `remove`, and `status`, so choose it deliberately
at first install.

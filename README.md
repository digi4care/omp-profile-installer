# omp-topologies

Portable OMP topologies: versioned overlays of agent definitions,
orchestration rules, and charter skills for
[OMP](https://github.com/can1357/oh-my-pi) agent configurations,
installed into named OMP profiles by the `omp-topology` CLI.

## Repository status

This repository currently provides the installer and core tooling only
— the `omp-topology` CLI and its supporting scripts, CI, and
documentation. No topology payloads ship yet.

Topologies will be introduced separately, each through its own pull
request. The [topologies/README.md](topologies/README.md) index starts
empty and will list each topology as it lands, with the exact version
and description taken from that topology's `topology.yml` manifest.

## What the tooling does

- Each topology is a versioned, self-contained overlay under
  `topologies/<name>/` with metadata in `topology.yml`.
- Overlays merge into a named OMP profile without touching unrelated
  settings; installs are dry-runnable, reversible, and never target
  every profile implicitly.
- Profiles are isolated, so separate topologies can coexist on one
  machine.

## Quick start

Install the `omp-topology` CLI with the official signed-release
bootstrap (installs into `~/.local/bin` by default, no sudo):

```sh
curl -fsSL https://raw.githubusercontent.com/digi4care/omp-topologies/main/scripts/install-release.sh | sh
```

The bootstrap downloads the platform release asset, verifies the
RSA/SHA-256 signature of `SHA256SUMS` and the asset checksum, and
refuses to install on any failure. See [RELEASING.md](RELEASING.md) for
the signature model and release procedure.

> **Release availability:** the bootstrap requires a signed GitHub
> release with assets. If no release has been published yet, the script
> fails closed and installs nothing — check the
> [releases page](https://github.com/digi4care/omp-topologies/releases)
> first.

The bootstrap is the initial install path for the CLI; an installed CLI
can later update itself with the signed `omp-topology self-update`
command (same release-asset signature verification as the bootstrap).
The CLI installs *topologies*, not itself — installing a topology is a
separate `omp-topology install` step. Installed binaries have no
bundled checkout, so pass the GitHub source explicitly. Once a topology
is published (see [topologies/README.md](topologies/README.md)), preview
the install with `--dry-run`:

```sh
omp-topology install -t <topology-name> \
  --source github:digi4care/omp-topologies \
  --profile <profile-name> \
  --dry-run
```

If the plan looks right, rerun without `--dry-run`, then start OMP with
that profile:

```sh
omp-topology install -t <topology-name> \
  --source github:digi4care/omp-topologies \
  --profile <profile-name>
omp --profile <profile-name>
```

For a shorter command, ask OMP to create a shell alias for the profile:

```sh
omp --profile <profile-name> --alias <alias-name>
```

> **Profile-specific setup:** a named profile has its own provider
> authentication, settings, and user-level OMP plugins and commands.
> Sign in to each model provider, and install user-scoped plugins or
> add user-level slash commands separately in every profile.
>
> Avoid copying OAuth credentials between profiles. When a provider
> rotates refresh tokens and detects reuse, refreshing one copy can
> leave another profile with a stale token; reusing it may revoke the
> current token and require signing in again. Authenticate separately
> with OAuth in each profile.

## More documentation

- [topologies/README.md](topologies/README.md) — topology index with
  exact manifest versions and descriptions.
- [CONTRIBUTING.md](CONTRIBUTING.md) — contribution, verification, and
  pull-request guidance.
- [docs/](docs/) — design notes, plans, and research documents.
- [RELEASING.md](RELEASING.md) — release signing and procedure.

## License

Apache License 2.0. See [LICENSE](LICENSE); attribution notes are in
[NOTICE](NOTICE) (Copyright 2026 Chris Engelhard).

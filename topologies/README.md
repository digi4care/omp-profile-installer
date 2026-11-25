# Topologies

This directory holds the installable OMP topologies in this
repository. Each topology is a self-contained payload in its own
`topologies/<name>/` subdirectory, declaring its name, version, and
description in a `topology.yml` manifest and documenting itself in its
own `README.md`. Topologies are introduced separately, each through
its own pull request.

There is no hand-maintained index here: the source tree itself is the
catalog, so a new topology never has to touch this file. To see which
topologies are currently available, browse the subdirectories and
their manifests, or run the CLI from a checkout:

```sh
omp-topology list
```

or, from anywhere, against the published source:

```sh
omp-topology list --source github:digi4care/omp-topologies
```

Add `--json` for machine-readable output. The command reports each
installable topology's name, version, and description straight from
its manifest, and reports none when no topology payload is present.

Install instructions are in the
[root README](../README.md#-quick-start). To propose a topology, see
[CONTRIBUTING.md](../CONTRIBUTING.md).

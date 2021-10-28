`gha-runner` runs Github Actions workflows. You can run workflows locally, or extend `gha-runner` with custom backends to run workflows elsewhere (e.g. [Pernosco](https://pernos.co) uses `gha-runner` to run Github Actions workflows in [ECS](https://aws.amazon.com/ecs/)). You can also customize the execution of workflows, e.g. wrapping the execution of workflow steps with various tools.

## Getting Started

```
git clone https://github.com/Pernosco/gha-runner
cd gha-runner
cargo build --examples
target/debug/examples/gha_local --image-path ghcr.io/catthehacker/ubuntu:act Pernosco github-actions-test 6475d0f048a72996e3bd559cdd3763f53fe3d072 .github/workflows/build.yml "Build+test (stable, ubuntu-18.04)"
```
The first time you run this it will download the 1GB docker image [`ghcr.io/catthehacker/ubuntu:act-18.04`](https://github.com/catthehacker/docker_images/blob/master/README.md); this may take a while on a slow connection.

## Status

Very simple workflows are supported. Many Github Actions features are not yet supported, e.g.
* Action steps that use their own containers
* Much of the [expression syntax](https://docs.github.com/en/actions/learn-github-actions/expressions)
* Pre/post actions
* Workflows that run their own docker commands

PRs welcome!

## Instrumenting workflows

To demonstrate this capability, the `gha_local` example has an `--strace` option that runs the action steps under [`strace`](https://man7.org/linux/man-pages/man1/strace.1.html).
```
target/debug/examples/gha_local --strace /tmp/strace.out --image-path ghcr.io/catthehacker/ubuntu:js Pernosco github-actions-test 6475d0f048a72996e3bd559cdd3763f53fe3d072 .github/workflows/build.yml "Build+test (stable, ubuntu-18.04)"
```

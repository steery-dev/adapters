#!/usr/bin/env bash
# Builds every adapter for the targets it runs on, packs them into a release
# (PROTOCOL.md §7.1 in steery-dev/sdk) and, with --publish, publishes it on GitHub.
# An operating system's adapter is built only for that system's targets; an adapter
# that makes hosts for every target.
#
#   ./release.sh <tag> [--publish] [--no-pack]
#
# Needs cargo-zigbuild and zig, the four targets installed with rustup, and `steery`
# on PATH (or STEERY=/path/to/steery) for the pack step.
set -euo pipefail

usage() { echo "usage: $0 <tag> [--publish] [--no-pack]" >&2; exit 2; }
[[ $# -ge 1 ]] || usage
tag=$1; shift
publish=0 pack=1
for arg in "$@"; do
  case $arg in
    --publish) publish=1 ;;
    --no-pack) pack=0 ;;
    *) usage ;;
  esac
done

root=$(cd "$(dirname "$0")" && pwd)
cd "$root"

# Linux builds link glibc 2.30 at most, so they run in every current Linux image.
targets=(aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu.2.30 aarch64-unknown-linux-gnu.2.30)
# Each crate and the systems it is built for: `apple`, `linux` or `all`.
crates=(macos:apple linux:linux daytona:all opensandbox:all joined:all)

rm -rf dist
for zig_target in "${targets[@]}"; do
  target=${zig_target%.2.30}
  case $target in *-apple-darwin) os=apple ;; *) os=linux ;; esac
  mkdir -p "dist/$target"
  # zig 0.16 refuses the `-oso_prefix` rustc passes for Apple targets; a Mac links its own
  # targets with Apple's linker.
  build=(cargo zigbuild)
  [[ $os == apple && $(uname -s) == Darwin ]] && build=(cargo build)
  for entry in "${crates[@]}"; do
    crate=${entry%%:*} on=${entry#*:}
    [[ $on == all || $on == "$os" ]] || continue
    (cd "$crate" && "${build[@]}" --release --locked --target "$zig_target")
    cp "$crate/target/$target/release/steery-adapter-$crate" "dist/$target/"
  done
done

[[ $pack -eq 1 ]] || exit 0
folders=()
for zig_target in "${targets[@]}"; do
  target=${zig_target%.2.30}
  folders+=("$target=dist/$target")
done
"${STEERY:-steery}" programs pack --release "$tag" --out dist/release "${folders[@]}"

[[ $publish -eq 1 ]] || exit 0
# Releases run side by side: tag the commit this build came from, never the default
# branch's tip, which another release may have moved.
commit=$(git rev-parse HEAD)
git fetch --quiet origin
[[ -n $(git branch -r --contains "$commit") ]] || { echo "release.sh: push $commit first" >&2; exit 1; }
gh release create "$tag" --title "$tag" --target "$commit" dist/release/*

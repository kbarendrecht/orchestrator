#!/usr/bin/env bash
# Does the .deb this build produced actually install and run?
#
#   tools/apt-check.sh <path to the .deb> [version]
#
# **It runs itself in the container.** The `docker run` was written out at each
# call site and the two had already drifted — one copied the package to a fixed
# path first, one mounted it in place, and each derived the version its own way.
# The image, the mounts and the entrypoint are this script's business, so it owns
# them: a workflow step is one line, and the gate can be run by hand, which is what
# a call site written in YAML can never be.
#
# **`dpkg-deb -c` lists a package; it does not install one.** That listing is what
# `bundle.yml` had, and it cannot see a dependency the package fails to declare, a
# maintainer script that exits non-zero, or a binary that will not start on a
# clean machine. Every one of those reaches a user as "the app does not open",
# which is the class of fault #16 and #26 both were.
#
# **And it is the one place the apt install can be classified at all.** `classify`
# keys on the executable living under `/usr/bin`, so no sandbox on a developer's
# machine can be an apt install — a real install in a real container can, which is
# why the daemon says what installed it at start and this asserts that line.
#
# Run here rather than in `kbarendrecht/apt`: that repository installs what has
# **already been published**, which is the right check for the index and the wrong
# moment for the package. This one runs before the tag exists.
set -euo pipefail

say() { printf '\n\033[36m▸\033[0m %s\n' "$1"; }
die() { printf '\033[31m✗\033[0m %s\n' "$1" >&2; exit 1; }

# Outside the container: mount what was asked about and run this same file inside.
# `/.dockerenv` is the one file every container has and no host does.
if [ ! -f /.dockerenv ]; then
  deb=${1:?usage: apt-check.sh <path to the .deb> [version]}
  [ -f "$deb" ] || die "no such package: $deb"
  # The version to expect: given, or read off the workspace manifest beside this
  # script — one spelling of a read that was written three different ways.
  want=${2:-$(sed -n 's/^version = "\(.*\)"$/\1/p' "$(dirname "$0")/../Cargo.toml" | head -1)}
  [ -n "$want" ] || die "no version to check against"
  exec docker run --rm \
    -v "$(cd "$(dirname "$deb")" && pwd)/$(basename "$deb")":/tmp/o.deb:ro \
    -v "$(cd "$(dirname "$0")" && pwd)/$(basename "$0")":/check.sh:ro \
    -e WANT="$want" \
    ubuntu:22.04 bash /check.sh
fi

say "installing the package"
export DEBIAN_FRONTEND=noninteractive
# `apt-get install ./file.deb`, never `dpkg -i`: the second one does not resolve
# the dependencies the bundler declared, so it would pass on a package that no
# user can install.
#
# Twice, because the first attempt fails for a reason that is not ours often
# enough to matter: the image ships an index, a mirror rotates a point release out
# from under it, and the fetch 404s. A gate that goes red on somebody else's
# mirror is one people learn to re-run without reading, which is the habit this
# repo spends its checks avoiding.
install_it() {
  apt-get update -qq && apt-get install -y -qq /tmp/o.deb git curl >/dev/null
}
install_it || { say "retrying after a fresh index"; install_it; } \
  || die "the package would not install"

say "what it put on the machine"
for want in orchd orch orchestrator-desktop; do
  [ -x "/usr/bin/$want" ] || die "the package did not install /usr/bin/$want"
  echo "  /usr/bin/$want"
done

say "what it says it is"
have=$(orchd --version | awk '{print $2}')
echo "  orchd --version → $have"
[ "$have" = "${WANT:?the wanted version}" ] || die "installed $have, built $WANT"

# A repository for the daemon to open. Nothing is done in it: what is under test
# is the start, not the work.
say "starting it the way a machine with no display would"
git config --global init.defaultBranch main
git config --global user.email check@example.com
git config --global user.name check
repo=$(mktemp -d)/repo
mkdir -p "$repo"
git -C "$repo" init -q
git -C "$repo" commit -q --allow-empty -m one

state=$(mktemp -d)
log=$(mktemp)
# `--main` points it at the repo; the config directory keeps every file it writes
# inside the container's scratch.
ORCHD_CONFIG_DIR="$state" orchd --main "$repo" >"$log" 2>&1 &
daemon=$!
trap 'kill "$daemon" 2>/dev/null || true' EXIT

for _ in $(seq 1 100); do
  grep -q 'token=' "$log" && break
  sleep 0.2
done
grep -q 'token=' "$log" || { cat "$log"; die "the daemon never served"; }

url=$(grep -o 'http://127.0.0.1:[0-9]*/?token=[a-z0-9]*' "$log" | head -1)
port=$(echo "$url" | sed -E 's#.*:([0-9]+)/.*#\1#')
token=$(echo "$url" | sed -E 's#.*token=##')
curl -fsS -H "x-orch-token: $token" "http://127.0.0.1:$port/api/state" >/dev/null \
  || { cat "$log"; die "the daemon served nothing on $port"; }
echo "  it serves on $port"

say "and what it thinks installed it"
# The whole reason this runs in a container: `/usr/bin` is what makes this an apt
# install, and only a real install puts it there.
grep -q 'installed by.*apt package' "$log" \
  || { grep 'installed by' "$log" || true; die "it does not know it is an apt install"; }
echo "  the apt package"

printf '\n\033[32m✔\033[0m the .deb installs, runs, and knows what it is\n'

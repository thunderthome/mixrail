#!/bin/sh
# Builds the Debian/Ubuntu (.deb, on Ubuntu 24.04) and Fedora (.rpm) packages in clean
# containers, then installs each in a fresh container to check it works. Needs Docker.
# Results go to dist/.
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
owner="$(id -u):$(id -g)"
mkdir -p dist
tar --exclude=./target --exclude=./.git --exclude=./dist \
    --transform "s,^\.,mixrail-$version," -czf "dist/mixrail-$version.tar.gz" .

# A tiny layout for the smoke test (`check` reads the config, no sound server needed).
cat > dist/smoke.toml <<'TOML'
[[strip]]
id = "music"
name = "Music"
kind = "virtual"
buses = ["b1"]

[[bus]]
id = "b1"
name = "Virtual Mic"
kind = "virtual"
TOML
smoke='mixrail --version && mixrail check --config /dist/smoke.toml && test -f /usr/lib/systemd/user/mixrail.service && test -f /usr/share/applications/mixrail.desktop && echo SMOKE-OK'

echo "== .deb (Ubuntu 24.04)"
docker run --rm -v "$PWD/dist:/dist" ubuntu:24.04 sh -euc "
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq curl build-essential clang libclang-dev pkg-config libpipewire-0.3-dev libasound2-dev >/dev/null
  curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal
  . \$HOME/.cargo/env
  cargo install -q cargo-deb
  cd /tmp && tar xzf /dist/mixrail-$version.tar.gz && cd mixrail-$version
  cargo deb
  cp target/debian/*.deb /dist/
  chown $owner /dist/*.deb"
docker run --rm -v "$PWD/dist:/dist" ubuntu:24.04 sh -euc "
  apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq /dist/mixrail_${version}-1_amd64.deb >/dev/null
  $smoke"

echo "== .rpm (Fedora)"
docker run --rm -v "$PWD/dist:/dist" fedora:latest sh -euc "
  dnf install -y -q rpm-build cargo rust clang-devel pkgconf pipewire-devel alsa-lib-devel systemd-rpm-macros >/dev/null
  mkdir -p ~/rpmbuild/SOURCES && cp /dist/mixrail-$version.tar.gz ~/rpmbuild/SOURCES/
  tar xzf /dist/mixrail-$version.tar.gz -C /tmp
  rpmbuild -ba --quiet /tmp/mixrail-$version/packaging/rpm/mixrail.spec
  cp ~/rpmbuild/RPMS/*/mixrail-*.rpm ~/rpmbuild/SRPMS/mixrail-*.src.rpm /dist/
  chown $owner /dist/*.rpm"
docker run --rm -v "$PWD/dist:/dist" fedora:latest sh -euc "
  dnf install -y -q \$(ls /dist/mixrail-$version-*.rpm | grep -v src.rpm) >/dev/null
  $smoke"

rm dist/smoke.toml
ls -l dist

#!/usr/bin/env bash
# User-local toolchain for cross-compiling MultiGateway to macOS from Linux.
# Does not use sudo. Prints the env a later build should export.
set -euo pipefail

prefix="${HOME}/.local"
cache="${HOME}/.cache/multigateway"
mkdir -p "$prefix/bin" "$cache" "$HOME/.cargo" "$HOME/.rustup"
export PATH="${prefix}/bin:${HOME}/.cargo/bin:/usr/bin:/bin"
# WSL often cannot reach the public internet directly when a local proxy owns
# the host routes. Clash's mixed port is enough for rustup, git and curl.
if [[ -z "${https_proxy:-}" ]] && curl -fsS --max-time 5 -x http://127.0.0.1:7890 -o /dev/null https://github.com; then
  export http_proxy=http://127.0.0.1:7890
  export https_proxy=http://127.0.0.1:7890
  export HTTP_PROXY=http://127.0.0.1:7890
  export HTTPS_PROXY=http://127.0.0.1:7890
  echo "using http://127.0.0.1:7890"
fi
export RUSTUP_HOME="${HOME}/.rustup"
export CARGO_HOME="${HOME}/.cargo"

fetch() {
  local url="$1" dest="$2"
  if [[ -f "$dest" ]]; then
    echo "cached $dest"
    return
  fi
  echo "downloading $url"
  curl --proto '=https' --tlsv1.2 -fL --retry 3 --retry-delay 2 -o "$dest.partial" "$url"
  mv "$dest.partial" "$dest"
}

if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal --no-modify-path
fi
# shellcheck disable=SC1091
source "${HOME}/.cargo/env"
rustup target add aarch64-apple-darwin x86_64-apple-darwin

if [[ ! -x "${prefix}/bin/zig" ]]; then
  index="${cache}/zig-index.json"
  fetch "https://ziglang.org/download/index.json" "$index"
  python3 - "$index" "$cache" "$prefix" <<'PY'
import json, pathlib, sys, tarfile, urllib.request
index_path, cache, prefix = sys.argv[1:]
data = json.loads(pathlib.Path(index_path).read_text())
versions = []
for key, value in data.items():
    if key == "master" or not isinstance(value, dict):
        continue
    asset = value.get("x86_64-linux") or {}
    if "tarball" in asset:
        versions.append(key)
versions.sort(key=lambda item: [int(part) if part.isdigit() else part for part in item.split(".")])
version = versions[-1]
asset = data[version]["x86_64-linux"]
tarball = pathlib.Path(cache) / f"zig-{version}.tar.xz"
print(f"zig {version} {asset['tarball']}", flush=True)
if not tarball.exists():
    urllib.request.urlretrieve(asset["tarball"], tarball)
extract = pathlib.Path(cache) / "zig"
if extract.exists():
    import shutil
    shutil.rmtree(extract)
extract.mkdir()
with tarfile.open(tarball) as archive:
    archive.extractall(extract)
inner = next(extract.iterdir())
dest = pathlib.Path(prefix) / "zig"
if dest.exists():
    import shutil
    shutil.rmtree(dest)
inner.rename(dest)
bin_link = pathlib.Path(prefix) / "bin" / "zig"
bin_link.unlink(missing_ok=True)
bin_link.symlink_to(dest / "zig")
PY
fi
zig version

if [[ ! -x "${prefix}/bin/cmake" ]]; then
  cmake_tar="${cache}/cmake-linux.tar.gz"
  fetch "https://github.com/Kitware/CMake/releases/download/v3.31.8/cmake-3.31.8-linux-x86_64.tar.gz" "$cmake_tar"
  rm -rf "${cache}/cmake"
  mkdir -p "${cache}/cmake"
  tar -xzf "$cmake_tar" -C "${cache}/cmake" --strip-components=1
  ln -sfn "${cache}/cmake/bin/cmake" "${prefix}/bin/cmake"
fi
cmake --version | head -1

if [[ ! -d "${cache}/MacOSX15.5.sdk" ]]; then
  sdk_tar="${cache}/MacOSX15.5.sdk.tar.xz"
  fetch "https://github.com/joseluisq/macosx-sdks/releases/download/15.5/MacOSX15.5.sdk.tar.xz" "$sdk_tar"
  tar -xJf "$sdk_tar" -C "$cache"
fi
if [[ ! -d "${cache}/MacOSX15.5.sdk" ]]; then
  echo "MacOSX SDK archive did not contain MacOSX15.5.sdk" >&2
  find "$cache" -maxdepth 2 -type d -name 'MacOSX*.sdk' -print >&2
  exit 1
fi

if ! command -v cargo-zigbuild >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -fsSL \
    https://github.com/rust-cross/cargo-zigbuild/releases/download/v0.23.4/cargo-zigbuild-installer.sh \
    | sh
fi
command -v cargo-zigbuild

if [[ ! -x "${prefix}/bin/rcodesign" ]]; then
  sign_tar="${cache}/rcodesign.tar.gz"
  fetch "https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign/0.29.0/apple-codesign-0.29.0-x86_64-unknown-linux-musl.tar.gz" "$sign_tar"
  tar -xzf "$sign_tar" -C "${cache}"
  found="$(find "${cache}" -type f -name rcodesign -print -quit)"
  install -m 755 "$found" "${prefix}/bin/rcodesign"
fi
rcodesign --version

if [[ ! -x "${prefix}/bin/dmg" ]]; then
  src="${cache}/libdmg-hfsplus"
  if [[ ! -d "$src/.git" ]]; then
    rm -rf "$src"
    git clone --depth 1 https://github.com/fanquake/libdmg-hfsplus.git "$src"
  fi
  cmake -S "$src" -B "${src}/build" -DCMAKE_BUILD_TYPE=Release
  cmake --build "${src}/build" --target dmg-bin -j"$(nproc)"
  built="$(find "${src}/build" -type f -name dmg -executable -print -quit)"
  install -m 755 "$built" "${prefix}/bin/dmg"
fi
dmg 2>&1 | head -20 || true

cat <<EOF
BOOTSTRAP_OK
export PATH="${prefix}/bin:${HOME}/.cargo/bin:/usr/bin:/bin"
export RUSTUP_HOME="${HOME}/.rustup"
export CARGO_HOME="${HOME}/.cargo"
export SDKROOT="${cache}/MacOSX15.5.sdk"
export MACOSX_DEPLOYMENT_TARGET=11.0
EOF

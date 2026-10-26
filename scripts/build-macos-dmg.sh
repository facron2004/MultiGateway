#!/usr/bin/env bash
# Build single-architecture macOS apps and pack each into its own DMG.
#
# The disk image is an ordinary APFS volume created by hdiutil, because the
# layout is what determines whether macOS can mount it and copy the app out.
# Packing the .app into an ISO and re-wrapping it with libdmg-hfsplus produces
# a UDZO image whose compressed blocks end up shuffled inside the data fork;
# macOS then reads every partition from the wrong offsets, reports the DMG as
# not recognized, and the app can never be installed. That path is gone.
#
#   macOS        builds and packages with the system toolchain (hdiutil, codesign)
#   Linux/WSL    cross-compiles with cargo-zigbuild and takes the finished DMG
#                from a macOS runner instead of fabricating one
#
# Output: dist/MultiGateway-<version>-macos-<arm64|x64>.dmg
# Every DMG is verified by scripts/verify-macos-dmg.py before the script exits.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
version="$(awk -F'"' '/^version = /{print $2; exit}' "$root/Cargo.toml")"
dist="$root/dist"
plist_template="$root/packaging/macos/Info.plist"
icon_png="$root/crates/mg-server/ui/icons/multigateway.png"
work="${MG_MACOS_WORK:-$HOME/.cache/multigateway}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
export PATH="${HOME}/.local/bin:${CARGO_HOME:-$HOME/.cargo}/bin:${PATH}"

host="$(uname -s)"

if [[ -z "$version" ]]; then
  echo "Could not read version from Cargo.toml" >&2
  exit 1
fi

# WSL often cannot reach the internet directly when a local proxy owns the routes.
if [[ -z "${https_proxy:-}" ]] && curl -fsS --max-time 5 -x http://127.0.0.1:7890 -o /dev/null https://github.com 2>/dev/null; then
  export http_proxy=http://127.0.0.1:7890
  export https_proxy=http://127.0.0.1:7890
  export HTTP_PROXY=http://127.0.0.1:7890
  export HTTPS_PROXY=http://127.0.0.1:7890
fi

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Missing command: $1" >&2
    exit 1
  }
}

verify_macho() {
  local bin="$1" expected="$2"
  python3 - "$bin" "$expected" <<'PY'
import struct, sys
path, expected = sys.argv[1], int(sys.argv[2], 16)
with open(path, "rb") as handle:
    header = handle.read(8)
magic, cputype = struct.unpack("<II", header)
if magic in (0xCAFEBABE, 0xBEBAFECA):
    raise SystemExit(f"{path} is a universal binary; this package must be a single architecture")
if magic != 0xFEEDFACF:
    raise SystemExit(f"{path} is not a 64-bit Mach-O (magic {magic:#x})")
if cputype != expected:
    raise SystemExit(f"{path} cpu type {cputype:#x}, expected {expected:#x}")
print(f"mach-o ok {path} cputype={cputype:#x}")
PY
}

make_icns() {
  local dest="$1"
  python3 - "$icon_png" "$dest" <<'PY'
import io, struct, sys
from PIL import Image
src = Image.open(sys.argv[1]).convert("RGBA")
# Modern icns entries store PNG payloads. Sizes follow Apple's icon types.
entries = [
    (b"icp4", 16),
    (b"icp5", 32),
    (b"ic11", 32),
    (b"icp6", 64),
    (b"ic12", 64),
    (b"ic07", 128),
    (b"ic13", 256),
    (b"ic08", 256),
    (b"ic14", 512),
    (b"ic09", 512),
    (b"ic10", 1024),
]
body = b""
for ostype, pixels in entries:
    buffer = io.BytesIO()
    src.resize((pixels, pixels), Image.Resampling.LANCZOS).save(buffer, format="PNG")
    payload = buffer.getvalue()
    body += ostype + struct.pack(">I", len(payload) + 8) + payload
blob = b"icns" + struct.pack(">I", len(body) + 8) + body
with open(sys.argv[2], "wb") as handle:
    handle.write(blob)
PY
}

stage_app() {
  local target="$1" stage="$2" label="$3" cputype="$4"
  local app="$stage/MultiGateway.app"
  local bin="$root/target/$target/release/MultiGateway"
  [[ -f "$bin" ]] || {
    echo "Missing binary: $bin" >&2
    exit 1
  }
  verify_macho "$bin" "$cputype"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  cp "$bin" "$app/Contents/MacOS/MultiGateway"
  chmod 755 "$app/Contents/MacOS/MultiGateway"
  if [[ "$host" != "Darwin" ]]; then
    # Repair Zig's deployment target/signature only for cross-compiled binaries.
    python3 "$root/scripts/fix-macos-macho.py" "$app/Contents/MacOS/MultiGateway"
  fi
  sed "s/__VERSION__/${version}/g" "$plist_template" > "$app/Contents/Info.plist"
  cp "$work/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
  printf 'APPL????' > "$app/Contents/PkgInfo"
  printf '%s' "$label" > "$stage/.arch"
  cat > "$stage/使用说明.txt" <<EOF
MultiGateway ${version}（macOS ${label} 版）

这个磁盘映像只包含 ${label} 的程序，不是通用二进制。
请安装和这台 Mac 芯片匹配的那个 DMG。

安装：把 MultiGateway.app 拖到“应用程序”文件夹，然后打开。

程序没有用 Apple 开发者证书签名，第一次打开会被系统拦下，属于正常现象：
  方法一（推荐）：在“应用程序”里按住 Control 点 MultiGateway，选“打开”，再点“打开”。
  方法二：打开“系统设置 → 隐私与安全性”，在底部点“仍要打开”。
  方法三：终端执行
      sudo xattr -dr com.apple.quarantine /Applications/MultiGateway.app

如果提示“已损坏，无法打开”，说明 DMG 在下载或制作时损坏了，请重新下载；
不要用“终端打开”这类命令绕过完整性检查。

账号和配置默认保存在：
~/Library/Application Support/MultiGateway
如果把 gateway-data 文件夹放在 MultiGateway.app 旁边，程序会改用那个目录。

卸载：先退出菜单栏里的 MultiGateway，再把程序拖到废纸篓。

需要 macOS 11 或更新版本。
EOF
}

sign_and_package() {
  local stage="$1" label="$2" cputype="$3"
  local app="$stage/MultiGateway.app"
  local output="$dist/MultiGateway-${version}-macos-${label}.dmg"

  if [[ "$host" == "Darwin" ]]; then
    # Ad-hoc signature: required on Apple silicon and after the plist edits above.
    codesign --force --options runtime --sign - --timestamp=none "$app/Contents/MacOS/MultiGateway"
    codesign --force --sign - "$app"
    codesign --verify --deep --strict --verbose=2 "$app"
  else
    need rcodesign
    rcodesign sign "$app"
  fi

  mkdir -p "$dist"
  rm -f "$output"
  create_dmg "$stage" "$output"
  python3 "$root/scripts/verify-macos-dmg.py" "$output"
}

create_dmg() {
  local stage="$1" output="$2"
  if [[ "$host" == "Darwin" ]]; then
    # A plain APFS volume keeps the app's bundle, symlink and extended
    # attributes intact; UDZO then compresses it the way Apple expects.
    hdiutil create -volname "MultiGateway" -srcfolder "$stage" -fs APFS \
      -format UDZO -ov "$output"
    hdiutil verify "$output"
    return
  fi
  echo "Refusing to fabricate a DMG off macOS." >&2
  echo "A container written by libdmg-hfsplus from an ISO stores its compressed blocks" >&2
  echo "out of order, and macOS cannot mount or install the result." >&2
  echo "Build the DMG on a Mac (or download the macOS artifact from GitHub Actions):" >&2
  echo "    bash scripts/build-macos-dmg.sh $( [[ "$(cat "$stage/.arch" 2>/dev/null)" == Intel* ]] && echo x86_64-apple-darwin || echo aarch64-apple-darwin )" >&2
  echo "or set MG_ALLOW_UNVERIFIED_CONTAINER=1 to produce one anyway for inspection." >&2
  if [[ "${MG_ALLOW_UNVERIFIED_CONTAINER:-}" != "1" ]]; then
    exit 1
  fi
  need genisoimage
  need dmg
  local iso
  iso="$(mktemp --suffix=.iso)"
  genisoimage -R -J -V "MultiGateway" -o "$iso" "$stage"
  dmg "$iso" "$output"
  rm -f "$iso"
}

build_target() {
  local target="$1"
  if [[ "$host" == "Darwin" ]]; then
    cargo build --locked --release -p mg-server --target "$target" --manifest-path "$root/Cargo.toml"
    return
  fi
  if [[ -z "${SDKROOT:-}" && -d "${work}/MacOSX15.5.sdk" ]]; then
    export SDKROOT="${work}/MacOSX15.5.sdk"
  fi
  [[ -n "${SDKROOT:-}" && -d "$SDKROOT" ]] || {
    echo "Set SDKROOT to a MacOSX.sdk directory before cross-compiling." >&2
    exit 1
  }
  need cargo
  need zig
  cargo zigbuild --locked --release -p mg-server --target "$target" --manifest-path "$root/Cargo.toml"
}

mkdir -p "$work"
if [[ -f "$root/packaging/macos/AppIcon.icns" ]]; then
  cp "$root/packaging/macos/AppIcon.icns" "$work/AppIcon.icns"
else
  make_icns "$work/AppIcon.icns"
fi

case "${1:-all}" in
  all) targets=(aarch64-apple-darwin x86_64-apple-darwin) ;;
  aarch64-apple-darwin|x86_64-apple-darwin) targets=("$1") ;;
  *) echo "Usage: $0 [aarch64-apple-darwin|x86_64-apple-darwin|all]" >&2; exit 1 ;;
esac
rustup target add "${targets[@]}"

for target in "${targets[@]}"; do
  if [[ "$target" == "aarch64-apple-darwin" ]]; then
    label=arm64; cputype=0x0100000c; audience="Apple 芯片（M 系列）"
  else
    label=x64; cputype=0x01000007; audience="Intel 芯片"
  fi
  if [[ "${MG_PACKAGE_ONLY:-}" != "1" ]]; then
    build_target "$target"
  fi

  if [[ "$host" != "Darwin" && "${MG_ALLOW_UNVERIFIED_CONTAINER:-}" != "1" ]]; then
    # Producing an unverifiable image on a non-Mac host is the bug that shipped
    # in 0.11.2; stop before it wastes a build.
    echo "Skipping $label: DMGs must be produced on macOS." >&2
    echo "  - run the macOS job of .github/workflows/build.yml, or" >&2
    echo "  - build on a Mac with: bash scripts/build-macos-dmg.sh $target" >&2
    echo "The cross-compiled binary in target/$target/release/MultiGateway is still usable" >&2
    echo "for smoke tests; only the disk image needs a Mac." >&2
    continue
  fi

  stage="$(mktemp -d)"
  trap 'rm -rf "${stage:-}"' EXIT
  stage_app "$target" "$stage" "$audience" "$cputype"
  sign_and_package "$stage" "$label" "$cputype"
  rm -rf "$stage"
  trap - EXIT
  echo "Wrote $dist/MultiGateway-${version}-macos-${label}.dmg"
done

echo "Done: DMGs are in $dist"

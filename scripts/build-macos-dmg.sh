#!/usr/bin/env bash
# Build two single-architecture macOS apps and pack each into its own DMG.
# On macOS this uses the system toolchain. On Linux it cross-compiles with
# cargo-zigbuild and a MacOSX SDK (SDKROOT).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
version="$(awk -F'"' '/^version = /{print $2; exit}' "$root/Cargo.toml")"
dist="$root/dist"
plist_template="$root/packaging/macos/Info.plist"
icon_png="$root/crates/mg-server/ui/icons/multigateway.png"
work="${MG_MACOS_WORK:-$HOME/.cache/multigateway}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
export PATH="${HOME}/.local/bin:${CARGO_HOME:-$HOME/.cargo}/bin:${PATH}"
if [[ -z "${https_proxy:-}" ]] && curl -fsS --max-time 5 -x http://127.0.0.1:7890 -o /dev/null https://github.com; then
  export http_proxy=http://127.0.0.1:7890
  export https_proxy=http://127.0.0.1:7890
  export HTTP_PROXY=http://127.0.0.1:7890
  export HTTPS_PROXY=http://127.0.0.1:7890
fi

if [[ -z "$version" ]]; then
  echo "Could not read version from Cargo.toml" >&2
  exit 1
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
if magic == 0xCAFEBABE or magic == 0xBEBAFECA:
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
    buf = io.BytesIO()
    src.resize((pixels, pixels), Image.Resampling.LANCZOS).save(buf, format="PNG")
    payload = buf.getvalue()
    body += ostype + struct.pack(">I", len(payload) + 8) + payload
blob = b"icns" + struct.pack(">I", len(body) + 8) + body
with open(sys.argv[2], "wb") as handle:
    handle.write(blob)
PY
}

sign_app() {
  local app="$1"
  if [[ "$(uname -s)" == "Darwin" ]]; then
    codesign --force --sign - "$app/Contents/MacOS/MultiGateway"
    codesign --force --sign - "$app"
  else
    need rcodesign
    rcodesign sign "$app"
  fi
}

create_dmg() {
  local stage="$1" output="$2"
  rm -f "$output"
  if [[ "$(uname -s)" == "Darwin" ]]; then
    hdiutil create -volname "MultiGateway" -srcfolder "$stage" -ov -format UDZO "$output"
    return
  fi
  need genisoimage
  need dmg
  local iso
  iso="$(mktemp --suffix=.iso)"
  # Rock Ridge keeps the executable bit and the Applications symlink.
  genisoimage -R -J -V "MultiGateway" -o "$iso" "$stage"
  dmg "$iso" "$output"
  rm -f "$iso"
}

package_one() {
  local target="$1" label="$2" cputype="$3" audience="$4"
  local bin="$root/target/$target/release/MultiGateway"
  local stage output app
  [[ -f "$bin" ]] || {
    echo "Missing binary: $bin" >&2
    exit 1
  }
  verify_macho "$bin" "$cputype"
  stage="$(mktemp -d)"
  app="$stage/MultiGateway.app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  cp "$bin" "$app/Contents/MacOS/MultiGateway"
  chmod 755 "$app/Contents/MacOS/MultiGateway"
  if [[ "$(uname -s)" != "Darwin" ]]; then
    # Repair Zig's deployment target/signature only for cross-compiled binaries.
    python3 "$root/scripts/fix-macos-macho.py" "$app/Contents/MacOS/MultiGateway"
  fi
  sed "s/__VERSION__/${version}/g" "$plist_template" > "$app/Contents/Info.plist"
  cp "$work/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
  printf 'APPL????' > "$app/Contents/PkgInfo"
  ln -s /Applications "$stage/Applications"
  cat > "$stage/使用说明.txt" <<EOF
MultiGateway ${version}（macOS ${label}）

这个磁盘映像只包含 ${audience} 的程序，不是通用二进制。
请安装和这台 Mac 芯片匹配的那个 DMG。

把 MultiGateway.app 拖到“应用程序”文件夹后再打开。

第一次打开如果系统提示无法验证开发者：
在“应用程序”里按住 Control 点 MultiGateway，选择“打开”，再确认打开。

账号和配置默认保存在：
~/Library/Application Support/MultiGateway
如果把 gateway-data 文件夹放在 MultiGateway.app 旁边，程序会改用那个目录。

需要 macOS 11 或更新版本。
EOF
  sign_app "$app"
  if [[ "$(uname -s)" == "Darwin" ]]; then
    codesign --verify --deep --strict "$app"
  elif [[ "$label" == "arm64" ]]; then
    python3 "$root/scripts/fix-macos-macho.py" --check "$app/Contents/MacOS/MultiGateway"
  fi
  mkdir -p "$dist"
  output="$dist/MultiGateway-${version}-macos-${label}.dmg"
  create_dmg "$stage" "$output"
  rm -rf "$stage"
  echo "Wrote $output"
}

build_target() {
  local target="$1"
  if [[ "$(uname -s)" == "Darwin" ]]; then
    cargo build --locked --release -p mg-server --target "$target" --manifest-path "$root/Cargo.toml"
  else
    if [[ -z "${SDKROOT:-}" && -d "${HOME}/.cache/multigateway/MacOSX15.5.sdk" ]]; then
    export SDKROOT="${HOME}/.cache/multigateway/MacOSX15.5.sdk"
  fi
  [[ -n "${SDKROOT:-}" && -d "$SDKROOT" ]] || {
      echo "Set SDKROOT to a MacOSX.sdk directory before cross-compiling." >&2
      exit 1
    }
    need cargo
    need zig
    cargo zigbuild --locked --release -p mg-server --target "$target" --manifest-path "$root/Cargo.toml"
  fi
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
  if [[ "${MG_PACKAGE_ONLY:-}" != "1" ]]; then
    build_target "$target"
  fi
  if [[ "$target" == "aarch64-apple-darwin" ]]; then
    package_one "$target" arm64 0x0100000c "Apple 芯片（M 系列）"
  else
    package_one "$target" x64 0x01000007 "Intel 芯片"
  fi
done

echo "Done: DMGs are in $dist"

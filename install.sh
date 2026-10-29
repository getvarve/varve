#!/usr/bin/env bash
# Install Varve from GitHub releases: the app on macOS, an AppImage with a
# desktop entry on Linux. https://github.com/getvarve/varve

if [ -z "${BASH_VERSION:-}" ]; then
  echo "error: install.sh requires bash" >&2
  exit 1
fi

set -euo pipefail

repo="getvarve/varve"
github="https://github.com/${repo}"

# Varve brand: the violet of the year-rings mark (assets/logo, packages/ui tokens.css).
stops=(91 75 224 139 124 255 179 169 255)
basic_codes=(35 35 35)
accent_rgb="139;124;255"
accent_256=141
tagline="A calm desktop manager for PostgreSQL, MySQL, MariaDB and Redis."
logo_block=(
  '██╗   ██╗ █████╗ ██████╗ ██╗   ██╗███████╗'
  '██║   ██║██╔══██╗██╔══██╗██║   ██║██╔════╝'
  '██║   ██║███████║██████╔╝██║   ██║█████╗  '
  '╚██╗ ██╔╝██╔══██║██╔══██╗╚██╗ ██╔╝██╔══╝  '
  ' ╚████╔╝ ██║  ██║██║  ██║ ╚████╔╝ ███████╗'
  '  ╚═══╝  ╚═╝  ╚═╝╚═╝  ╚═╝  ╚═══╝  ╚══════╝'
)
logo_ascii=(
  '__     __     _      ____   __     __  _____ '
  '\ \   / /    / \    |  _ \  \ \   / / | ____|'
  ' \ \ / /    / _ \   | |_) |  \ \ / /  |  _|  '
  '  \ V /    / ___ \  |  _ <    \ V /   | |___ '
  '   \_/    /_/   \_\ |_| \_\    \_/    |_____|'
)

# Presentation: logo, step lines, spinner, and a summary box. Runs on bash 3.2 (macOS).
tty=0
if [ -t 1 ]; then
  tty=1
  # Keep the terminal on fd 3: inside $(...) stdout is a pipe, and with curl | bash so is stdin.
  exec 3>&1
fi

color=1
if [ -n "${NO_COLOR:-}" ] || [ "${TERM:-}" = "dumb" ] || { [ "$tty" -eq 0 ] && [ -z "${CLICOLOR_FORCE:-}" ]; }; then
  color=0
fi

# Unicode art needs a UTF-8 locale and a bash that counts characters, not bytes.
unicode=0
probe="✓"
case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in
  *[Uu][Tt][Ff]-8* | *[Uu][Tt][Ff]8*)
    if [ "${#probe}" -eq 1 ]; then
      unicode=1
    fi
    ;;
  "")
    # No locale at all is common in fresh macOS shells. Try a UTF-8 one for this script only;
    # an explicit C or POSIX locale keeps the ASCII fallback.
    for candidate in en_US.UTF-8 C.UTF-8; do
      LC_CTYPE="$candidate"
      if [ "${#probe}" -eq 1 ]; then
        unicode=1
        break
      fi
    done
    if [ "$unicode" -eq 0 ]; then
      unset LC_CTYPE
    fi
    ;;
esac

depth="basic"
case "${COLORTERM:-}" in
  truecolor | 24bit) depth="truecolor" ;;
  *)
    case "${TERM:-}" in
      *256color*) depth="256" ;;
    esac
    ;;
esac

if [ "$color" -eq 1 ]; then
  c_reset=$'\033[0m'
  c_bold=$'\033[1m'
  c_dim=$'\033[2m'
  c_red=$'\033[31m'
  c_green=$'\033[32m'
  c_yellow=$'\033[33m'
  case "$depth" in
    truecolor) c_accent=$'\033['"38;2;${accent_rgb}m" ;;
    256) c_accent=$'\033['"38;5;${accent_256}m" ;;
    *) c_accent=$'\033['"${basic_codes[1]}m" ;;
  esac
else
  c_reset=""
  c_bold=""
  c_dim=""
  c_red=""
  c_green=""
  c_yellow=""
  c_accent=""
fi

if [ "$unicode" -eq 1 ]; then
  mark_ok="✓"
  mark_warn="!"
  mark_err="✗"
  spinner_frames=("⠋" "⠙" "⠹" "⠸" "⠼" "⠴" "⠦" "⠧" "⠇" "⠏")
  box_tl="╭"
  box_tr="╮"
  box_bl="╰"
  box_br="╯"
  box_h="─"
  box_v="│"
  logo_rows=("${logo_block[@]}")
else
  mark_ok="+"
  mark_warn="!"
  mark_err="x"
  spinner_frames=("|" "/" "-" "\\")
  box_tl="+"
  box_tr="+"
  box_bl="+"
  box_br="+"
  box_h="-"
  box_v="|"
  logo_rows=("${logo_ascii[@]}")
fi

cursor_hidden=0

terminal_columns() {
  local size="" cols=""
  if [ "$tty" -eq 1 ]; then
    size="$(stty size <&3 2>/dev/null || true)"
    cols="${size#* }"
  fi
  case "$cols" in
    "" | *[!0-9]*) cols="${COLUMNS:-80}" ;;
  esac
  case "$cols" in
    "" | *[!0-9]*) cols=80 ;;
  esac
  printf '%s\n' "$cols"
}

# Sets $paint to the escape for column $1 of $2 on the logo gradient ($stops, three RGB stops).
gradient_escape() {
  local column="$1" span="$2" shadow="$3"
  local fraction=$((column * 1000 / (span > 1 ? span - 1 : 1)))
  local from_r from_g from_b to_r to_g to_b local_f
  if [ "$fraction" -lt 500 ]; then
    from_r=${stops[0]} from_g=${stops[1]} from_b=${stops[2]} to_r=${stops[3]} to_g=${stops[4]} to_b=${stops[5]}
    local_f=$((fraction * 2))
  else
    from_r=${stops[3]} from_g=${stops[4]} from_b=${stops[5]} to_r=${stops[6]} to_g=${stops[7]} to_b=${stops[8]}
    local_f=$(((fraction - 500) * 2))
  fi
  local r=$((from_r + (to_r - from_r) * local_f / 1000))
  local g=$((from_g + (to_g - from_g) * local_f / 1000))
  local b=$((from_b + (to_b - from_b) * local_f / 1000))
  local weight="1"
  if [ "$shadow" -eq 1 ]; then
    r=$((r * 55 / 100))
    g=$((g * 55 / 100))
    b=$((b * 55 / 100))
    weight="22"
  fi
  case "$depth" in
    truecolor)
      paint=$'\033['"${weight};38;2;${r};${g};${b}m"
      ;;
    256)
      local lr lg lb
      lr=$(xterm_level "$r")
      lg=$(xterm_level "$g")
      lb=$(xterm_level "$b")
      paint=$'\033['"${weight};38;5;$((16 + 36 * lr + 6 * lg + lb))m"
      ;;
    *)
      local code="${basic_codes[0]}"
      if [ "$fraction" -ge 670 ]; then
        code="${basic_codes[2]}"
      elif [ "$fraction" -ge 340 ]; then
        code="${basic_codes[1]}"
      fi
      if [ "$shadow" -eq 1 ]; then
        paint=$'\033[2;'"${code}m"
      else
        paint=$'\033[1;'"${code}m"
      fi
      ;;
  esac
}

xterm_level() {
  if [ "$1" -lt 48 ]; then
    printf '0'
  elif [ "$1" -lt 115 ]; then
    printf '1'
  else
    printf '%s' $((($1 - 35) / 40))
  fi
}

print_logo() {
  local span="${#logo_rows[0]}"
  local cols
  cols="$(terminal_columns)"
  printf '\n'
  if [ $((span + 2)) -le "$cols" ]; then
    local row line i ch shadow last=""
    for row in "${logo_rows[@]}"; do
      if [ "$color" -eq 0 ]; then
        printf '  %s\n' "$row"
        continue
      fi
      line=""
      i=0
      while [ "$i" -lt "${#row}" ]; do
        ch="${row:$i:1}"
        if [ "$ch" = " " ]; then
          line="${line} "
        else
          shadow=0
          if [ "$unicode" -eq 1 ] && [ "$ch" != "█" ]; then
            shadow=1
          fi
          gradient_escape "$i" "$span" "$shadow"
          if [ "$paint" != "$last" ]; then
            line="${line}${paint}"
            last="$paint"
          fi
          line="${line}${ch}"
        fi
        i=$((i + 1))
      done
      last=""
      printf '  %s%s\n' "$line" "$c_reset"
    done
    printf '\n\n'
  fi
  printf '  %s%s%s  %s\n\n' "$c_bold" "$tagline" "$c_reset" "${c_dim}installer${c_reset}"
}

step() {
  printf '  %s %-10s %s\n' "${c_green}${mark_ok}${c_reset}" "$1" "${c_dim}$2${c_reset}"
}

note() {
  printf '  %s %-10s %s\n' "${c_yellow}${mark_warn}${c_reset}" "$1" "$2"
}

error() {
  printf '  %s %s\n' "${c_bold}${c_red}${mark_err}${c_reset}" "${c_red}$*${c_reset}" >&2
}

die() {
  error "$@"
  exit 1
}

show_cursor() {
  if [ "$cursor_hidden" -eq 1 ]; then
    printf '\033[?25h'
    cursor_hidden=0
  fi
}

# Runs "$@" in the background with a spinner and label $1 when stdout is a terminal.
# Its stderr is held back and printed after the spinner line is cleared.
spin() {
  local label="$1"
  shift
  if [ "$tty" -eq 0 ]; then
    "$@"
    return
  fi
  # A label wider than the terminal would wrap, and \r would only redraw its last row.
  local room=$(($(terminal_columns) - 5))
  if [ "${#label}" -gt "$room" ] && [ "$room" -gt 3 ]; then
    label="${label:0:$((room - 3))}..."
  fi
  local log="${tmpdir}/spin.log"
  "$@" 2>"$log" &
  local pid=$!
  local frame=0
  local count="${#spinner_frames[@]}"
  printf '\033[?25l'
  cursor_hidden=1
  while kill -0 "$pid" 2>/dev/null; do
    printf '\r  %s %s' "${c_accent}${spinner_frames[$((frame % count))]}${c_reset}" "$label"
    frame=$((frame + 1))
    sleep 0.08
  done
  printf '\r\033[2K'
  show_cursor
  local status=0
  wait "$pid" || status=$?
  if [ "$status" -ne 0 ] && [ -s "$log" ]; then
    sed 's/^/    /' "$log" >&2
  fi
  return "$status"
}

display_path() {
  case "$1" in
    "${HOME}"/*) printf '~%s\n' "${1#"${HOME}"}" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

human_size() {
  awk -v bytes="$1" 'BEGIN {
    if (bytes >= 1048576) printf "%.1f MB", bytes / 1048576
    else if (bytes >= 1024) printf "%.0f KB", bytes / 1024
    else printf "%d B", bytes
  }'
}

# Prints a rounded box. Arguments alternate plain text and its colored form.
summary_box() {
  local width=0 plain
  local -a plains=() painted=()
  while [ "$#" -gt 0 ]; do
    if [ "$#" -gt 2 ] || [ -n "$1" ]; then
      plains+=("$1")
      painted+=("$2")
    fi
    shift 2
  done
  for plain in "${plains[@]}"; do
    if [ "${#plain}" -gt "$width" ]; then
      width="${#plain}"
    fi
  done
  local i=0
  if [ $((width + 10)) -gt "$(terminal_columns)" ]; then
    printf '\n'
    while [ "$i" -lt "${#painted[@]}" ]; do
      printf '  %s\n' "${painted[$i]}"
      i=$((i + 1))
    done
    printf '\n'
    return
  fi
  local rule=""
  while [ "$i" -lt $((width + 6)) ]; do
    rule="${rule}${box_h}"
    i=$((i + 1))
  done
  local blank
  blank="$(printf '%*s' $((width + 6)) '')"
  printf '\n  %s\n' "${c_dim}${box_tl}${rule}${box_tr}${c_reset}"
  printf '  %s%s%s\n' "${c_dim}${box_v}${c_reset}" "$blank" "${c_dim}${box_v}${c_reset}"
  i=0
  while [ "$i" -lt "${#plains[@]}" ]; do
    local pad=$((width - ${#plains[$i]}))
    printf '  %s   %s%*s   %s\n' "${c_dim}${box_v}${c_reset}" "${painted[$i]}" "$pad" '' "${c_dim}${box_v}${c_reset}"
    i=$((i + 1))
  done
  printf '  %s%s%s\n' "${c_dim}${box_v}${c_reset}" "$blank" "${c_dim}${box_v}${c_reset}"
  printf '  %s\n\n' "${c_dim}${box_bl}${rule}${box_br}${c_reset}"
}

if ! command -v curl >/dev/null 2>&1; then
  die "curl is required"
fi

# Every helper keeps its variables local: the install steps below use names like $dest.
download() {
  local url="$1" out="$2"
  curl -fsSL --retry 3 --retry-delay 2 --proto '=https' --tlsv1.2 -o "$out" "$url"
}

normalize_version() {
  local raw="$1"
  case "$raw" in
    "" ) die "empty version" ;;
    *[!A-Za-z0-9._-]*) die "invalid version: ${raw}" ;;
  esac
  case "$raw" in
    v*) printf '%s\n' "$raw" ;;
    *) printf 'v%s\n' "$raw" ;;
  esac
}

resolve_version() {
  if [ -n "${1:-}" ]; then
    normalize_version "$1"
    return
  fi
  if [ -n "${VARVE_VERSION:-}" ]; then
    normalize_version "$VARVE_VERSION"
    return
  fi
  local latest_url="${github}/releases/latest" effective="" tag
  if ! effective="$(curl -fsSL --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' "$latest_url")"; then
    die "could not find the latest release at ${latest_url}"
  fi
  case "$effective" in
    */releases/tag/*) ;;
    *) die "unexpected latest-release URL: ${effective}" ;;
  esac
  tag="${effective##*/}"
  normalize_version "$tag"
}

sha256_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{ print $1 }'
  else
    die "sha256sum or shasum is required to verify the download"
  fi
}

verify_checksum() {
  local file="$1" name="$2" sums_file="$3" expected actual
  # Exact name match: a regex would read the dots in the asset name as wildcards.
  expected="$(awk -v name="$name" '$2 == name || $2 == "*" name { print $1; exit }' "$sums_file")"
  if [ -z "$expected" ]; then
    die "checksum mismatch: SHA256SUMS has no entry for ${name}"
  fi
  expected="$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')"
  actual="$(sha256_file "$file" | tr '[:upper:]' '[:lower:]')"
  if [ "$actual" != "$expected" ]; then
    error "checksum mismatch for ${name}"
    error "expected ${expected}"
    error "actual   ${actual}"
    exit 1
  fi
}

app_name="Varve"
# Up to 0.1.7 the app was called Varve Community. It has the same bundle id, so the installer
# replaces a copy under the old name instead of installing Varve next to it.
legacy_name="Varve Community"
bundle_id="com.getvarve.desktop"

# Sets $platform (macos or linux) and $asset_arch (arm64 or x64).
detect_platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin)
      platform="macos"
      # A shell under Rosetta reports x86_64 on Apple silicon; the native build is faster.
      if [ "$arch" = "x86_64" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = "1" ]; then
        arch="arm64"
      fi
      ;;
    Linux)
      platform="linux"
      if [ -f /etc/alpine-release ]; then
        die "unsupported platform: musl libc (Alpine). The Linux builds require glibc."
      fi
      # The builds come from Ubuntu 22.04 runners, and an AppImage does not bundle glibc.
      local glibc
      glibc="$(getconf GNU_LIBC_VERSION 2>/dev/null || true)"
      glibc="${glibc##* }"
      if [ -n "$glibc" ] && ! version_at_least "$glibc" 2 35; then
        die "Varve needs glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36); this system has ${glibc}"
      fi
      ;;
    *)
      die "unsupported platform: ${os} ${arch}. Use install.ps1 on Windows."
      ;;
  esac
  case "$arch" in
    arm64 | aarch64) asset_arch="arm64" ;;
    x86_64 | amd64) asset_arch="x64" ;;
    *) die "unsupported platform: ${os} ${arch}" ;;
  esac
}

# True when version $1 (MAJOR.MINOR[...]) is at least $2.$3.
version_at_least() {
  local major="${1%%.*}" minor="${1#*.}"
  minor="${minor%%[!0-9]*}"
  case "$major" in "" | *[!0-9]*) return 1 ;; esac
  case "$minor" in "") minor=0 ;; esac
  [ "$major" -gt "$2" ] || { [ "$major" -eq "$2" ] && [ "$minor" -ge "$3" ]; }
}

# Prints the absolute path of directory $1 (CDPATH would make cd print it twice).
absolute_dir() {
  (CDPATH='' cd -P -- "$1" >/dev/null && pwd)
}

# Fails when a process runs a binary inside the app bundle $1 (named $2 in the message): replacing
# or trashing a running macOS app breaks it. -ww keeps long command lines whole. The quoted case pattern is a fixed string (a
# bracket or plus sign in the path is no pattern) and needs no pipe, which under pipefail would
# fail through SIGPIPE as soon as a grep -q stopped reading.
refuse_when_running() {
  local processes
  processes="$(ps -axww -o command= 2>/dev/null || true)"
  # The path must start a line or follow a space (an interpreter running a script from the
  # bundle), so a copy in ~/Applications does not match /Applications.
  case "
$processes" in
    *"
$1/Contents/MacOS/"* | *" $1/Contents/MacOS/"*) die "${2} is running. Quit it and run the installer again." ;;
  esac
}

# Prints the bundle id of the app bundle $1, or nothing.
bundle_id_of() {
  /usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$1/Contents/Info.plist" 2>/dev/null || true
}

# Moves $1 to the Trash with the Finder, so it can be put back. The path goes in as an argument,
# never as script text.
move_to_trash() {
  osascript \
    -e 'on run argv' \
    -e 'set doomed to POSIX file (item 1 of argv) as alias' \
    -e 'tell application "Finder" to delete doomed' \
    -e 'end run' \
    "$1" >/dev/null 2>&1 </dev/null
}

print_logo

platform=""
asset_arch=""
detect_platform
version="$(resolve_version "${1:-}")"
if [ "$platform" = "macos" ]; then
  asset="varve-macos-${asset_arch}.app.tar.gz"
else
  asset="varve-linux-${asset_arch}.AppImage"
fi
base="${github}/releases/download/${version}"

step "Version" "${version}"
step "Platform" "${platform} ${asset_arch}"

tmpdir=""
cleanup() {
  show_cursor
  if [ -n "$tmpdir" ]; then
    rm -rf "$tmpdir"
  fi
}
trap cleanup EXIT
tmpdir="$(mktemp -d)"

download_path="${tmpdir}/${asset}"
sums="${tmpdir}/SHA256SUMS"

if ! spin "Downloading ${asset}" download "${base}/${asset}" "$download_path"; then
  die "download failed: ${base}/${asset}"
fi
if ! download "${base}/SHA256SUMS" "$sums"; then
  die "download failed: ${base}/SHA256SUMS"
fi
step "Download" "${asset} ($(human_size "$(wc -c <"$download_path" | tr -d ' ')"))"

verify_checksum "$download_path" "$asset" "$sums"
step "Checksum" "SHA-256 matches SHA256SUMS"

notes_plain=()
notes_painted=()

install_macos() {
  local apps_dir found="" name
  if [ -n "${VARVE_INSTALL:-}" ]; then
    apps_dir="${VARVE_INSTALL%/}"
  else
    # Upgrade an existing copy where it is, so a rerun never leaves a second one behind. A copy
    # under the old name counts as well.
    for name in "$app_name" "$legacy_name"; do
      if [ -d "/Applications/${name}.app" ]; then
        found="/Applications"
      elif [ -d "${HOME}/Applications/${name}.app" ]; then
        found="${HOME}/Applications"
      fi
      if [ -n "$found" ]; then
        break
      fi
    done
    if [ "$found" = "/Applications" ] && [ ! -w /Applications ]; then
      die "${name} is installed in /Applications by another account. Run this as an admin, or set VARVE_INSTALL=~/Applications."
    fi
    if [ -n "$found" ]; then
      apps_dir="$found"
    elif [ -w /Applications ]; then
      apps_dir="/Applications"
    else
      apps_dir="${HOME}/Applications"
    fi
  fi
  if ! mkdir -p "$apps_dir"; then
    die "could not create ${apps_dir}"
  fi
  apps_dir="$(absolute_dir "$apps_dir")"

  local extract="${tmpdir}/extract"
  mkdir -p "$extract"
  tar -xzf "$download_path" -C "$extract"
  local src="${extract}/${app_name}.app"
  if [ ! -d "$src" ]; then
    # Releases up to 0.1.7 hold the app under its old name. It is installed as Varve.app all the
    # same: macOS does not go by the bundle's file name.
    src="${extract}/${legacy_name}.app"
  fi
  if [ ! -d "$src" ]; then
    die "archive does not contain ${app_name}.app"
  fi

  dest="${apps_dir}/${app_name}.app"
  refuse_when_running "$dest" "$app_name"
  # The old app in the same folder is replaced, but only when it really is Varve.
  local legacy="${apps_dir}/${legacy_name}.app"
  if [ -d "$legacy" ] && [ "$(bundle_id_of "$legacy")" = "$bundle_id" ]; then
    refuse_when_running "$legacy" "$legacy_name"
  else
    legacy=""
  fi

  # Stage next to the destination so the final step is a rename on one volume.
  local stage="${apps_dir}/.${app_name}.app.new"
  local old="${apps_dir}/.${app_name}.app.old"
  rm -rf "$stage" "$old"
  cp -R "$src" "$stage"
  # curl does not set the quarantine flag, but an earlier browser download may have.
  xattr -dr com.apple.quarantine "$stage" 2>/dev/null || true
  if [ -e "$dest" ]; then
    mv "$dest" "$old"
  fi
  if ! mv "$stage" "$dest"; then
    if [ -e "$old" ]; then
      mv "$old" "$dest"
    fi
    die "could not install ${dest}"
  fi
  rm -rf "$old"
  step "Installed" "$(display_path "$dest")"

  if [ -n "$legacy" ]; then
    if move_to_trash "$legacy"; then
      step "Replaced" "$(display_path "$legacy"), now in the Trash"
    else
      note "Replaced" "could not move $(display_path "$legacy") to the Trash"
      notes_plain+=("Note      ${legacy_name}.app is still there; move it to the Trash")
      notes_painted+=("${c_dim}Note${c_reset}      ${legacy_name}.app is still there; move it to the Trash")
    fi
  fi

  installed="${version#v}"
  local plist_version
  if plist_version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "${dest}/Contents/Info.plist" 2>/dev/null)"; then
    installed="$plist_version"
  fi
  next_plain="open -a \"${app_name}\""
  next_painted="${c_bold}${c_accent}${next_plain}${c_reset} ${c_dim}or find it in Launchpad${c_reset}"
  next_plain="${next_plain} or find it in Launchpad"
}

install_linux() {
  local data_home="${XDG_DATA_HOME:-${HOME}/.local/share}"
  local app_dir="${VARVE_INSTALL:-${data_home}/varve}"
  app_dir="${app_dir%/}"
  if ! mkdir -p "$app_dir"; then
    die "could not create ${app_dir}"
  fi
  app_dir="$(absolute_dir "$app_dir")"

  # Replacing the file of a running AppImage is safe: the running copy keeps the old inode.
  dest="${app_dir}/Varve.AppImage"
  local stage="${dest}.new"
  cp "$download_path" "$stage"
  chmod 755 "$stage"
  mv -f "$stage" "$dest"
  step "Installed" "$(display_path "$dest")"

  # The icon is a release asset too, so the checksum covers it.
  local icon_asset="varve-icon-128.png"
  local icon_tmp="${tmpdir}/${icon_asset}"
  local icon_dir="${data_home}/icons/hicolor/128x128/apps"
  local icon_line=""
  if download "${base}/${icon_asset}" "$icon_tmp" 2>/dev/null; then
    verify_checksum "$icon_tmp" "$icon_asset" "$sums"
    mkdir -p "$icon_dir"
    cp "$icon_tmp" "${icon_dir}/varve.png"
    icon_line="Icon=varve"
  else
    note "Icon" "could not download ${icon_asset}; the menu entry has no icon"
  fi

  local desktop_dir="${data_home}/applications"
  local desktop_file="${desktop_dir}/varve.desktop"
  mkdir -p "$desktop_dir"
  # Exec quoting (Desktop Entry spec): inside double quotes " ` $ \ take a backslash, and the
  # string escape rule runs first, so a literal backslash takes four and the others two.
  # A literal % is %%.
  local exec_path
  exec_path="$(printf '%s' "$dest" | sed -e 's/\\/\\\\\\\\/g' -e 's/["`$]/\\\\&/g' -e 's/%/%%/g')"
  {
    printf '[Desktop Entry]\n'
    printf 'Type=Application\n'
    printf 'Name=%s\n' "$app_name"
    printf 'Comment=%s\n' "$tagline"
    printf 'Exec="%s" %%U\n' "$exec_path"
    if [ -n "$icon_line" ]; then
      printf '%s\n' "$icon_line"
    fi
    printf 'Terminal=false\n'
    printf 'Categories=Development;Database;\n'
  } >"${desktop_file}.new"
  mv -f "${desktop_file}.new" "$desktop_file"
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$desktop_dir" >/dev/null 2>&1 || true
  fi
  step "Menu" "$(display_path "$desktop_file")"

  # A varve command on PATH, linked like the app menu entry: only into a directory that is
  # already on PATH, and never over a file that is not our own link.
  local link_dir candidate linked=""
  for link_dir in "${HOME}/.local/bin" "${HOME}/bin"; do
    case ":${PATH}:" in
      *":${link_dir}:"*) ;;
      *) continue ;;
    esac
    if [ ! -d "$link_dir" ] || [ ! -w "$link_dir" ]; then
      continue
    fi
    candidate="${link_dir}/varve"
    if [ -L "$candidate" ]; then
      if [ "$(readlink "$candidate")" != "$dest" ] && [ -e "$candidate" ]; then
        continue
      fi
    elif [ -e "$candidate" ]; then
      continue
    fi
    if ln -sfn "$dest" "$candidate" 2>/dev/null; then
      linked="$candidate"
      step "Linked" "$(display_path "$linked")"
    fi
    break
  done

  # AppImages mount themselves with FUSE 2. Without it they only start with an extra flag.
  local ldconfig_bin fuse=0
  ldconfig_bin="$(command -v ldconfig 2>/dev/null || true)"
  if [ -z "$ldconfig_bin" ] && [ -x /sbin/ldconfig ]; then
    ldconfig_bin=/sbin/ldconfig
  fi
  if [ -n "$ldconfig_bin" ]; then
    # No pipe into grep -q: under pipefail its early exit would read as "not found".
    case "$("$ldconfig_bin" -p 2>/dev/null || true)" in
      *libfuse.so.2*) fuse=1 ;;
    esac
  fi
  if [ "$fuse" -eq 0 ]; then
    note "FUSE" "libfuse2 not found; install it (Debian, Ubuntu: libfuse2t64 or libfuse2)"
    notes_plain+=("Note      without libfuse2, start it with --appimage-extract-and-run")
    notes_painted+=("${c_dim}Note${c_reset}      without libfuse2, start it with ${c_accent}--appimage-extract-and-run${c_reset}")
  fi

  installed="${version#v}"
  if [ -n "$linked" ]; then
    next_plain="varve, or open ${app_name} from your app menu"
    next_painted="${c_bold}${c_accent}varve${c_reset}, or open ${app_name} from your app menu"
  else
    next_plain="open ${app_name} from your app menu"
    next_painted="open ${c_bold}${app_name}${c_reset} from your app menu"
  fi
}

dest=""
installed=""
next_plain=""
next_painted=""
if [ "$platform" = "macos" ]; then
  install_macos
else
  install_linux
fi

title="${app_name} ${installed} is installed"
box_args=(
  "${mark_ok} ${title}" "${c_bold}${c_green}${mark_ok}${c_reset} ${c_bold}${title}${c_reset}"
  "" ""
  "Location  $(display_path "$dest")" "${c_dim}Location${c_reset}  $(display_path "$dest")"
  "Next      ${next_plain}" "${c_dim}Next${c_reset}      ${next_painted}"
)
i=0
while [ "$i" -lt "${#notes_plain[@]}" ]; do
  box_args+=("${notes_plain[$i]}" "${notes_painted[$i]}")
  i=$((i + 1))
done
summary_box "${box_args[@]}"

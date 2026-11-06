#!/usr/bin/env bash
# Builds a release binary and wraps it in a macOS .app, so it opens from
# Finder, the Dock or Spotlight without a Terminal window.
#
#   scripts/bundle.sh            # leaves the app in <target>/release/bundle/
#   scripts/bundle.sh --install  # moves it to ~/Applications instead, so
#                                # Spotlight finds a single copy
#
# An app opened from Finder does not see the shell's variables, so the
# GH_PR_STATUS_* ones set while bundling are written into Info.plist.
set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="PR Manager"
BINARY="gh-pr-status"
BUNDLE_ID="com.patriciogutierrez.pr-manager"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
APP="$TARGET_DIR/release/bundle/$APP_NAME.app"

cargo build --release

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$TARGET_DIR/release/$BINARY" "$APP/Contents/MacOS/$BINARY"
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"

xml_escape() {
  local text="$1"
  text="${text//&/&amp;}"
  text="${text//</&lt;}"
  text="${text//>/&gt;}"
  printf '%s' "$text"
}

# The settings come from the shell, or from .env when there is one, so a
# build without it sourced doesn't ship an app missing them.
if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi

ENVIRONMENT=""
for name in GH_PR_STATUS_BOT GH_PR_STATUS_GCP_PREPROD GH_PR_STATUS_GCP_PROD GH_PR_STATUS_GCP_DEVELOPMENT GH_PR_STATUS_SLACK_CHANNEL GH_PR_STATUS_SLACK_TEAM GH_PR_STATUS_CODE_DIR GH_PR_STATUS_REVIEW_WEBHOOK GH_PR_STATUS_SLACK_USER GH_PR_STATUS_REVIEWERS GH_PR_STATUS_REVIEW_CHANNELS GH_PR_STATUS_SLACK_TOKEN; do
  if [[ -n "${!name:-}" ]]; then
    ENVIRONMENT+="    <key>$name</key>
    <string>$(xml_escape "${!name}")</string>
"
  fi
done

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>$APP_NAME</string>
  <key>CFBundleDisplayName</key>
  <string>$APP_NAME</string>
  <key>CFBundleIdentifier</key>
  <string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key>
  <string>$BINARY</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleVersion</key>
  <string>$VERSION</string>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>LSEnvironment</key>
  <dict>
$ENVIRONMENT  </dict>
</dict>
</plist>
PLIST

# macOS draws a notification with the icon of the app that sent it, so the
# app carries its own copy of terminal-notifier, renamed and with the app's
# icon, and sends through it. Without terminal-notifier installed, the app
# falls back to whichever one it finds.
NOTIFIER_SRC="$(brew --prefix terminal-notifier 2>/dev/null)/terminal-notifier.app"
NOTIFIER="$APP/Contents/Helpers/$APP_NAME Notifier.app"
if [[ -d "$NOTIFIER_SRC" ]]; then
  mkdir -p "$APP/Contents/Helpers"
  cp -R "$NOTIFIER_SRC" "$NOTIFIER"
  chmod -R u+w "$NOTIFIER"
  rm -rf "$NOTIFIER/Contents/_CodeSignature" "$NOTIFIER/Contents/Resources/Terminal.icns"
  cp assets/AppIcon.icns "$NOTIFIER/Contents/Resources/AppIcon.icns"
  plist="$NOTIFIER/Contents/Info.plist"
  /usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $BUNDLE_ID.notifier" "$plist"
  /usr/libexec/PlistBuddy -c "Set :CFBundleName $APP_NAME" "$plist"
  /usr/libexec/PlistBuddy -c "Set :CFBundleIconFile AppIcon" "$plist"
  codesign --force --sign - "$NOTIFIER"
else
  echo "Sin terminal-notifier (brew install terminal-notifier): las notificaciones no llevan el ícono de la app"
fi

# Ad-hoc signature over the whole bundle, so the Info.plist is sealed with the
# binary. Enough for an app built and run on this same Mac.
codesign --force --sign - "$APP"

if [[ "${1:-}" == "--install" ]]; then
  mkdir -p "$HOME/Applications"
  rm -rf "$HOME/Applications/$APP_NAME.app"
  mv "$APP" "$HOME/Applications/"
  echo "Instalada en ~/Applications/$APP_NAME.app"
else
  echo "App lista en $APP"
fi

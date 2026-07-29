#!/usr/bin/env sh
set -eu

SOURCE_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SOURCE="$SOURCE_DIR/divit"
INSTALL_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"

if [ ! -f "$SOURCE" ]; then
  echo "divit was not found in $SOURCE_DIR. Extract the complete release archive first." >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
cp "$SOURCE" "$INSTALL_DIR/divit"
chmod +x "$INSTALL_DIR/divit"
ln -sf "divit" "$INSTALL_DIR/divits-opensource"
ln -sf "divit" "$INSTALL_DIR/opensource"

printf "\nDivit's OpenSource Tool was installed to %s/divit\n" "$INSTALL_DIR"
printf "Add that directory to PATH if needed, then run: divit\n"

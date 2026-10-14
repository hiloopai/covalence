#!/usr/bin/env bash
set -euo pipefail

# Keep the pinned executable on the editor's PATH and rooted against Nix GC.
direnv_executable="$(command -v direnv)"
if [[ "$(readlink -f "$HOME/.nix-profile/bin/direnv")" != "$direnv_executable" ]]; then
  nix-env --install "${direnv_executable%/bin/direnv}"
fi
hook='eval "$(direnv hook bash)"'
if ! grep -Fqx "$hook" "$HOME/.bashrc"; then
  printf '\n%s\n' "$hook" >> "$HOME/.bashrc"
fi
direnv allow .
./bin/glu build

#!/usr/bin/env bash
# Create the private GitHub repository for zapline and push the full history.
#
# Run it next to the downloaded zapline.bundle (it clones the bundle into
# ./zapline first) or from inside a zapline checkout.
#
#   ./publish-to-github.sh                       # private repo samsamurai301/zapline
#   ./publish-to-github.sh --owner my-org --name zapline --public
#   ./publish-to-github.sh --remote <git-url>    # push elsewhere, no repo creation
#
# Uses the GitHub CLI (gh) when installed; otherwise asks you to create the
# empty repository in the browser and pushes with git.
set -euo pipefail

OWNER="samsamurai301"
NAME="zapline"
VISIBILITY="private"
REMOTE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --owner) OWNER="$2"; shift 2 ;;
    --name) NAME="$2"; shift 2 ;;
    --public) VISIBILITY="public"; shift ;;
    --remote) REMOTE="$2"; shift 2 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "error: unknown argument $1" >&2; exit 2 ;;
  esac
done

info() { printf '\033[36m==> %s\033[0m\n' "$*"; }
fail() { printf '\033[31merror: %s\033[0m\n' "$*" >&2; exit 1; }

command -v git >/dev/null || fail "git is not installed"

# 1. Find the zapline working copy, or create it from the bundle.
script_dir="$(cd "$(dirname "$0")" && pwd)"
if root="$(git rev-parse --show-toplevel 2>/dev/null)" && [[ -f "$root/crates/zapline/Cargo.toml" ]]; then
  cd "$root"
  info "Using the zapline checkout at $root"
else
  bundle=""
  for candidate in "$script_dir/zapline.bundle" "$PWD/zapline.bundle"; do
    [[ -f "$candidate" ]] && { bundle="$candidate"; break; }
  done
  [[ -n "$bundle" ]] || fail "run this script inside a zapline checkout, or put it next to zapline.bundle"
  target="$(dirname "$bundle")/zapline"
  [[ ! -e "$target" ]] || fail "$target already exists; run the script from inside it, or move it away"
  info "Cloning $bundle into $target"
  git clone "$bundle" "$target"
  cd "$target"
fi

branch="$(git branch --show-current)"
[[ -n "$branch" ]] || fail "no branch is checked out"
if [[ -n "$(git status --porcelain)" ]]; then
  echo "warning: uncommitted changes will not be pushed:" >&2
  git status --short >&2
fi

# 2. Create the GitHub repository (unless an explicit remote was given).
url="${REMOTE:-https://github.com/$OWNER/$NAME.git}"
if [[ -z "$REMOTE" ]]; then
  if command -v gh >/dev/null; then
    if ! gh auth status >/dev/null 2>&1; then
      info "Logging in to GitHub"
      gh auth login --hostname github.com --git-protocol https --web || fail "GitHub login failed"
    fi
    if gh repo view "$OWNER/$NAME" >/dev/null 2>&1; then
      info "Repository $OWNER/$NAME already exists; pushing to it"
    else
      info "Creating $VISIBILITY repository $OWNER/$NAME"
      gh repo create "$OWNER/$NAME" "--$VISIBILITY" --disable-wiki \
        --description "A fast, batteries-included Rust web framework: FastAPI ergonomics, enterprise structure, top-tier performance" \
        || fail "could not create the repository (check the owner name and your permissions)"
    fi
    gh auth setup-git >/dev/null 2>&1 || true
  else
    new_repo="https://github.com/new?owner=$OWNER&name=$NAME&visibility=$VISIBILITY"
    echo
    echo "The GitHub CLI (gh) is not installed, so create the repository in the browser:"
    echo "  1. Open $new_repo"
    echo "  2. Keep it EMPTY: no README, no .gitignore, no license."
    echo "  3. Click 'Create repository'."
    (command -v open >/dev/null && open "$new_repo") || (command -v xdg-open >/dev/null && xdg-open "$new_repo") || true
    read -r -p "Press Enter once the empty repository exists " _
  fi
fi

# 3. Point origin at GitHub and push the full history.
if git remote get-url origin >/dev/null 2>&1; then
  git remote set-url origin "$url"
else
  git remote add origin "$url"
fi
info "Pushing $branch to $url"
git push -u origin "$branch:main"
git push origin --tags

echo
printf '\033[32mDone.\033[0m\n'
if [[ -z "$REMOTE" ]]; then
  echo "  Repository: https://github.com/$OWNER/$NAME"
  echo "  CI runs:    https://github.com/$OWNER/$NAME/actions"
fi

#!/bin/zsh -f
# Run as a script, not with `source`; values go to gh through stdin only.
unsetopt XTRACE VERBOSE
setopt ERREXIT NOUNSET PIPEFAIL
umask 077

typeset release_use_clipboard=false
if [[ ${1:-} == --clipboard ]]; then
  release_use_clipboard=true
  shift
fi
if (( $# > 1 )); then
  print -u2 -- 'Usage: zsh -f scripts/setup-release-secrets.zsh [--clipboard] [OWNER/REPO]'
  exit 1
fi
if [[ ! -t 0 ]]; then
  print -u2 -- 'Run this script in an interactive terminal.'
  exit 1
fi
if ! command -v gh >/dev/null 2>&1; then
  print -u2 -- 'Install GitHub CLI (gh), then run gh auth login.'
  exit 1
fi
if [[ $release_use_clipboard == true ]] && ! command -v pbpaste >/dev/null 2>&1; then
  print -u2 -- 'Clipboard mode requires macOS pbpaste.'
  exit 1
fi

typeset release_repo=${1:-TemMax/conductor-remote}
if [[ ! $release_repo =~ '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' ]]; then
  print -u2 -- 'The repository must be OWNER/REPO.'
  exit 1
fi
if ! gh auth status >/dev/null 2>&1; then
  print -u2 -- 'GitHub authentication failed. Run gh auth login and try again.'
  exit 1
fi
if ! gh repo view "$release_repo" --json id >/dev/null 2>&1; then
  print -u2 -- 'Cannot access the repository. Check its name and your GitHub access.'
  exit 1
fi

typeset release_tty_state
release_tty_state=$(stty -g)
trap 'stty "$release_tty_state" 2>/dev/null || true' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

upload_secret() {
  local secret_name=$1
  local +x secret_value=''
  local +x secret_file=''
  local +x clipboard_confirmation=''

  print -- "\n$secret_name"
  case $secret_name in
    BUILD_CERTIFICATE_BASE64|NOTARY_KEY_BASE64)
      print -- 'Use base64, or @/path/to/file (spaces allowed, no quotes).'
      ;;
    SPARKLE_PRIVATE_KEY)
      print -- 'Use the existing Sparkle private key as base64.'
      ;;
    *) print -- 'Use the value from your secret store.' ;;
  esac

  while true; do
    if [[ $release_use_clipboard == true ]]; then
      if ! IFS= read -r -s 'clipboard_confirmation?Copy the value, then press Enter: '; then
        print -u2 -- '\nInput cancelled.'
        return 1
      fi
      print
      if [[ -n $clipboard_confirmation ]]; then
        clipboard_confirmation=''
        print -u2 -- 'Do not paste into this prompt; copy the value and press Enter.'
        continue
      fi
      if ! secret_value=$(pbpaste 2>/dev/null); then
        print -u2 -- 'Cannot read the clipboard; try again.'
        continue
      fi
    else
      if ! IFS= read -r -s 'secret_value?Value (hidden): '; then
        print -u2 -- '\nInput cancelled.'
        return 1
      fi
      print
    fi
    if [[ -z $secret_value ]]; then
      print -u2 -- 'Empty value; try again.'
      continue
    fi

    case $secret_name in
      BUILD_CERTIFICATE_BASE64|NOTARY_KEY_BASE64)
        if [[ $secret_value == @* ]]; then
          secret_file=${secret_value#@}
          if [[ $secret_file == '~/'* ]]; then
            secret_file=$HOME/${secret_file#\~/}
          fi
          if [[ ! -f $secret_file || ! -r $secret_file ]] ||
             ! secret_value=$(base64 < "$secret_file" 2>/dev/null); then
            print -u2 -- 'Cannot read the file; try again.'
            continue
          fi
          secret_file=''
        fi
        ;;
    esac
    case $secret_name in
      BUILD_CERTIFICATE_BASE64|NOTARY_KEY_BASE64|SPARKLE_PRIVATE_KEY)
        secret_value=${secret_value//[[:space:]]/}
        if [[ -z $secret_value || ! $secret_value =~ '^[A-Za-z0-9+/]*={0,2}$' ]] ||
           (( ${#secret_value} % 4 != 0 )) ||
           ! builtin print -rn -- "$secret_value" | base64 --decode >/dev/null 2>&1; then
          print -u2 -- 'Invalid base64; try again.'
          continue
        fi
        ;;
    esac
    break
  done

  if ! builtin print -rn -- "$secret_value" |
       gh secret set "$secret_name" --app actions --repo "$release_repo" >/dev/null 2>&1; then
    secret_value=''
    print -u2 -- "Failed to save $secret_name. Check GitHub access and rerun the script."
    return 1
  fi
  secret_value=''
  print -- "Saved $secret_name."
}

print -- "Saving six Actions secrets to $release_repo. Existing values will be replaced."
if [[ $release_use_clipboard == true ]]; then
  print -- 'For each secret: copy it from your secret store, return here and press Enter.'
  print -- 'The clipboard is read only after Enter. Do not paste into the terminal.'
else
  print -- 'Input is invisible, including pasted text. Press Enter after each value.'
  print -- 'For long base64 values, use @/path/to/file or rerun with --clipboard on macOS.'
fi
for release_secret_name in BUILD_CERTIFICATE_BASE64 P12_PASSWORD NOTARY_KEY_BASE64 \
                           NOTARY_KEY_ID NOTARY_ISSUER_ID SPARKLE_PRIVATE_KEY; do
  upload_secret "$release_secret_name"
done
print -- '\nAll six release secrets saved.'

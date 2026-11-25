# termy started zsh with ZDOTDIR pointing here: put the user's ZDOTDIR back
# so their own startup files load as usual, then add the integration.
_termy_dir=$ZDOTDIR
if [[ -n ${TERMY_ZDOTDIR+x} ]]; then
  ZDOTDIR=$TERMY_ZDOTDIR
else
  unset ZDOTDIR
fi
unset TERMY_ZDOTDIR
[[ -f ${ZDOTDIR:-$HOME}/.zshenv ]] && source "${ZDOTDIR:-$HOME}/.zshenv"
[[ -o interactive ]] && source "$_termy_dir/termy.zsh"
unset _termy_dir

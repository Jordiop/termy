# termy shell integration: OSC 133 prompt marks and OSC 7 working directory.
[[ -n $_termy_loaded ]] && return
typeset -g _termy_loaded=1 _termy_status=0 _termy_ran=

_termy_save_status() { _termy_status=$? }

_termy_urlencode() {
  emulate -L zsh
  local LC_ALL=C out= c i
  for (( i = 1; i <= ${#1}; i++ )); do
    c=${1[i]}
    case $c in
      [-/._~A-Za-z0-9]) out+=$c ;;
      *) out+=$(printf '%%%02X' "'$c") ;;
    esac
  done
  print -rn -- $out
}

_termy_precmd() {
  [[ -n $_termy_ran ]] && print -n "\e]133;D;$_termy_status\a"
  _termy_ran=
  print -n "\e]7;file://$HOST$(_termy_urlencode $PWD)\a"
  # Themes rebuild PS1 in their own precmd hooks, which run before this one.
  [[ $PS1 == *$'\e]133;A'* ]] || PS1=$'%{\e]133;A\a%}'$PS1$'%{\e]133;B\a%}'
}

_termy_preexec() {
  _termy_ran=1
  print -n "\e]133;C\a"
}

# Installed on the first prompt, after .zshrc: the status must be read first
# and PS1 wrapped last, around whatever hooks the user's config added.
_termy_install() {
  precmd_functions=(_termy_save_status ${precmd_functions:#_termy_install} _termy_precmd)
  preexec_functions+=(_termy_preexec)
  _termy_precmd
  # Hooks after this one may still rewrite PS1 for this first prompt.
  print -n "\e]133;A\a"
}
precmd_functions+=(_termy_install)

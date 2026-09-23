# Wraps the hey binary so it can see the previous command and its exit status.
hey() {
    local __hey_rc=$?
    local __hey_cur __hey_prev
    # $history[$HISTCMD] is the line that invoked us; the entry before it is the
    # command of interest unless the line is `producer | hey`.
    __hey_cur=${history[$HISTCMD]}
    __hey_prev=${history[$((HISTCMD - 1))]}
    [[ -z $__hey_prev ]] && __hey_prev=$(fc -ln -1 -1 2>/dev/null)
    command hey --shell zsh --exit-code "$__hey_rc" \
        --last-command "$__hey_prev" --current-command "$__hey_cur" "$@"
}

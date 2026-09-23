# Wraps the hey binary so it can see the previous command and its exit status.
hey() {
    local __hey_rc=$?
    local __hey_cur __hey_prev
    # Inside a function, `fc` counts back from the line that invoked us, so
    # `fc -ln -1 -1` is the last saved command before this one. `history 1` is
    # the newest entry: the invoking line itself, unless history skipped it
    # (HISTCONTROL=ignorespace and friends). The binary works out which is which.
    __hey_prev=$(HISTTIMEFORMAT= fc -ln -1 -1 2>/dev/null)
    __hey_cur=$(HISTTIMEFORMAT= builtin history 1 2>/dev/null)
    __hey_prev=${__hey_prev#"${__hey_prev%%[![:space:]]*}"}
    __hey_cur=${__hey_cur#"${__hey_cur%%[![:space:]]*}"}
    __hey_cur=${__hey_cur#"${__hey_cur%%[![:digit:]]*}"}
    __hey_cur=${__hey_cur#"${__hey_cur%%[![:space:]]*}"}
    command hey --shell bash --exit-code "$__hey_rc" \
        --last-command "$__hey_prev" --current-command "$__hey_cur" "$@"
}

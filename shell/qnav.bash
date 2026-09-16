# Source this file to allow Enter in qnav to change this shell's directory.
if ! declare -F qnav >/dev/null; then
qnav() {
    if [ "$#" -eq 1 ]; then
        case "$1" in --help|-h|--version|-V) command qnav "$@"; return $? ;; esac
    fi
    local qnav_file qnav_code qnav_version qnav_action qnav_path qnav_extra
    qnav_file=$(umask 077; command mktemp "${TMPDIR:-/tmp}/qnav.XXXXXXXXXX") || return 1
    if command qnav --result-file "$qnav_file" "$@"; then qnav_code=0; else qnav_code=$?; fi
    if [ "$qnav_code" -eq 0 ]; then
        if {
            IFS= read -r -d '' qnav_version &&
            IFS= read -r -d '' qnav_action &&
            IFS= read -r -d '' qnav_path &&
            ! IFS= read -r -d '' qnav_extra &&
            [ -z "$qnav_extra" ] &&
            [ "$qnav_version" = qnav-v1 ] && [ "$qnav_action" = cd ] &&
            [[ "$qnav_path" = /* ]]
        } 3<"$qnav_file" <&3; then
            builtin cd -- "$qnav_path" || qnav_code=$?
        else
            builtin printf '%s\n' 'qnav: invalid or missing shell result' >&2
            qnav_code=1
        fi
    fi
    command rm -f -- "$qnav_file"
    return "$qnav_code"
}
fi

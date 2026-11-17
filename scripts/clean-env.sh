# Sourced by scripts/test and scripts/repo-guard, in POSIX sh before either re-runs itself
# under bash, and by scripts/test again under bash.

# What Windows programs need to start at all. Every environment the scripts construct passes
# them on Windows.
windows_start_vars='SYSTEMROOT SYSTEMDRIVE WINDIR COMSPEC PATHEXT NUMBER_OF_PROCESSORS PROCESSOR_ARCHITECTURE OS'

# clean_exec <names> <command> [<arg>...]: replaces the shell with <command> in an environment
# that holds only the variables named in the space-separated <names> that are set, plus
# windows_start_vars on Windows.
clean_exec() {
  clean_names=$1
  shift
  case $(uname -s) in
    MINGW* | MSYS* | CYGWIN*) clean_names="$clean_names $windows_start_vars" ;;
  esac
  for clean_name in $clean_names; do
    if clean_value=$(printenv "$clean_name"); then
      set -- "$clean_name=$clean_value" "$@"
    fi
  done
  exec env -i "$@"
}

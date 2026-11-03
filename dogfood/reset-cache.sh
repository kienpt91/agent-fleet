#!/bin/sh
# Housekeeping of the volumes of the container wrapper (container-run.example.sh). A person runs it; no task can.
#
#   reset-cache.sh --scratch [--docker D] [--prefix P]
#       remove the scratch target volumes that a check left behind (a check killed with SIGKILL cannot clean up
#       after itself). A volume that a running check still uses is not removed: docker refuses, the script says so.
#   reset-cache.sh --warm [--docker D] [--volume V] [--image I]
#       throw the WARM target volume away and make an empty one for your user, for when you no longer trust it
#       (a check that ran something it should not have, a surprising build result). Then warm it again from a
#       commit you have reviewed (RUNBOOK.md, section 2). Checks start from a copy of the warm target and never
#       write to it, so this is housekeeping and a precaution, not something a task makes necessary.
#
# The names are those of the wrapper: scratch volumes start with `fleet-dogfood-scratch`, the warm volume is
# `fleet-dogfood-target`, the image `fleet-dev`; `--docker` is the absolute path of your docker client (default: `docker`).
set -eu
usage() { echo "usage: reset-cache.sh (--scratch | --warm) [--docker D] [--prefix P] [--volume V] [--image I]" >&2; exit 2; }
docker=docker; prefix=fleet-dogfood-scratch; warm=fleet-dogfood-target; image=fleet-dev; scratch=0; wipe=0
while [ $# -gt 0 ]; do
  case $1 in
    --scratch) scratch=1; shift ;;
    --warm) wipe=1; shift ;;
    --docker|--prefix|--volume|--image)
      [ $# -ge 2 ] || usage
      case $1 in --docker) docker=$2 ;; --prefix) prefix=$2 ;; --volume) warm=$2 ;; --image) image=$2 ;; esac
      shift 2 ;;
    *) usage ;;
  esac
done
[ $((scratch + wipe)) -eq 1 ] || usage
# only names of the shape the wrapper makes are ever removed
case $prefix in *[!A-Za-z0-9_.-]*|'') echo "reset-cache: odd --prefix" >&2; exit 2 ;; esac
case $warm in *[!A-Za-z0-9_.-]*|'') echo "reset-cache: odd --volume" >&2; exit 2 ;; esac
if [ "$scratch" -eq 1 ]; then
  removed=0; busy=0
  for v in $("$docker" volume ls -q --filter "name=$prefix"); do
    case $v in "$prefix"-*) ;; *) continue ;; esac
    if "$docker" volume rm "$v" >/dev/null 2>&1; then removed=$((removed + 1)); else busy=$((busy + 1)); echo "reset-cache: $v is in use or could not be removed; left alone" >&2; fi
  done
  echo "reset-cache: removed $removed scratch volume(s), left $busy"
else
  "$docker" volume rm "$warm" >/dev/null
  "$docker" volume create "$warm" >/dev/null
  "$docker" run --rm -u 0 --network none --cap-drop ALL --cap-add CHOWN --security-opt no-new-privileges \
    -v "$warm:/target" "$image" chown "$(id -u):$(id -g)" /target
  echo "reset-cache: the warm target $warm is empty; warm it again from a reviewed commit (RUNBOOK.md, section 2)"
fi

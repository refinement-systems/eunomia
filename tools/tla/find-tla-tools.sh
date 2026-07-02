# SPDX-License-Identifier: 0BSD
# intentionally no shebang - for sourcing, not executing

# Permission to use, copy, modify, and/or distribute this software for
# any purpose with or without fee is hereby granted.
#
# THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
# WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
# OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
# FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
# DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
# AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
# OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

# Source this file to export:
#   TLA_TOOLS : classpath entry containing tlc2.TLC, tla2sany.SANY, ...
#               (a standalone tla2tools.jar, or any dir holding the tools).
#   JAVA      : path to a `java` launcher (the TLA+ tools require Java 11+;
#               Java 17 is recommended, matching the upstream build).
# Works when sourced from a bash script; returns non-zero on failure so
# callers using `set -e` abort cleanly.
#
# Resolution order (first match wins):
#   1. Caller-provided TLA_TOOLS + JAVA  -> trust them (CI / explicit override).
#   2. Vendored ./tla2tools.jar + a system JDK (java on PATH or $JAVA_HOME).
#      The recommended, fully native path. Run ./fetch-tools.sh once to
#      download the jar.
# If neither resolves, fail with a message pointing at ./fetch-tools.sh.

# --- 1. explicit override (CI / explicit caller) ---------------------------
if [ -n "${TLA_TOOLS:-}" ] && [ -n "${JAVA:-}" ]; then
  export TLA_TOOLS JAVA
  return 0 2>/dev/null || exit 0
fi

# Directory holding this script (resolved when sourced from a bash script).
_HERE="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"

# --- 2. vendored jar + system JDK (preferred) ------------------------------
if [ -z "${TLA_TOOLS:-}" ] && [ -f "$_HERE/tla2tools.jar" ]; then
  TLA_TOOLS="$_HERE/tla2tools.jar"
fi

if [ -z "${JAVA:-}" ]; then
  if [ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/java" ]; then
    JAVA="$JAVA_HOME/bin/java"
  elif [ -x /usr/libexec/java_home ] && _JH="$(/usr/libexec/java_home 2>/dev/null)" \
       && [ -x "$_JH/bin/java" ]; then
    # macOS: resolve the real JDK rather than the /usr/bin/java shim.
    JAVA="$_JH/bin/java"
  elif command -v java >/dev/null 2>&1; then
    JAVA="$(command -v java)"
  fi
  unset _JH 2>/dev/null || true
fi

if [ -n "${TLA_TOOLS:-}" ] && [ -n "${JAVA:-}" ]; then
  unset _HERE
  export TLA_TOOLS JAVA
  return 0 2>/dev/null || exit 0
fi

# --- neither override nor the vendored jar + a system JDK resolved ----------
if [ -z "${TLA_TOOLS:-}" ]; then
  echo "ERROR: no TLA+ tools found." >&2
  echo "  Run '$_HERE/fetch-tools.sh' to vendor tla2tools.jar." >&2
fi

if [ -z "${JAVA:-}" ]; then
  echo "ERROR: no Java runtime found." >&2
  echo "  Install a JDK (e.g. 'brew install --cask temurin@17')." >&2
fi

unset _HERE
return 1 2>/dev/null || exit 1

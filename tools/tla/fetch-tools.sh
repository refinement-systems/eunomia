#!/bin/bash

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

# Download a standalone tla2tools.jar next to this script and record its SHA1,
# so the TLA+ command-line tools no longer depend on the (deprecated, x86_64)
# TLA+ Toolbox.app. The jar is pure Java and runs natively on any JDK 11+.
#
# Override the release tag with TLA_TOOLS_TAG, e.g.
#   TLA_TOOLS_TAG=nightly ./fetch-tools.sh
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"

# v1.8.0 ("Clarke") is a rolling pre-release: its tla2tools.jar asset is
# replaced on every push to master. The recorded .sha1 below is therefore your
# real version pin -- commit it so the toolchain is reproducible.
TAG="${TLA_TOOLS_TAG:-v1.8.0}"
URL="https://github.com/tlaplus/tlaplus/releases/download/${TAG}/tla2tools.jar"
DEST="$HERE/tla2tools.jar"

echo "Fetching $URL"
curl -fL --proto '=https' -o "$DEST" "$URL"

# Guard against silently downloading an HTML error page instead of a jar.
if command -v unzip >/dev/null 2>&1 && ! unzip -l "$DEST" >/dev/null 2>&1; then
  echo "ERROR: downloaded file is not a valid jar (got an error page?)." >&2
  rm -f "$DEST"
  exit 1
fi

shasum "$DEST" | tee "$DEST.sha1"
echo "Vendored: $DEST"
echo "You can now uninstall the TLA+ Toolbox; the scripts use this jar."

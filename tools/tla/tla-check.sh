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

# Run TLA+ syntax checker (SANY) on a .tla file.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=find-tla-tools.sh
source "$SCRIPT_DIR/find-tla-tools.sh"

SPEC="${1:-}"
if [ -z "$SPEC" ]; then
    echo "Usage: $(basename "$0") <spec.tla>" >&2
    exit 1
fi

SPEC_ABS="$(cd "$(dirname "$SPEC")" && pwd)/$(basename "$SPEC")"
SPEC_DIR="$(dirname "$SPEC_ABS")"
SPEC_BASE="$(basename "$SPEC_ABS")"

echo "Checking syntax: $SPEC_BASE"
cd "$SPEC_DIR"
"$JAVA" -cp "$TLA_TOOLS" tla2sany.SANY "$SPEC_BASE"

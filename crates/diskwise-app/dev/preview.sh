#!/bin/sh
# Screenshot the real UI with fixture data, without running the app.
#   DISKWISE_SNAPSHOT=snap.json dev/preview.sh name 'query' [dark|light] [name 'query' ...]
# Writes dev/shots/<name>-<theme>.png. Needs Google Chrome.
set -e
cd "$(dirname "$0")"
if [ -n "$DISKWISE_SNAPSHOT" ] && [ ! -f fixture.js ]; then
  (cd .. && cargo test -q -p diskwise-app write_preview_fixture -- --ignored >/dev/null)
fi
sed -e 's#href="style.css"#href="../ui/style.css"#' \
    -e 's#<script src="app.js"></script>#<script src="fixture.js"></script><script src="mock.js"></script><script src="../ui/app.js"></script>#' \
    ../ui/index.html > preview.html
mkdir -p shots
CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
while [ $# -ge 3 ]; do
  name=$1 query=$2 theme=$3; shift 3
  flags=""
  # Only the prefers-color-scheme setting: no automatic page darkening.
  # Blink's preferredColorScheme: 0 = dark, 1 = light (headless otherwise follows the OS).
  if [ "$theme" = dark ]; then flags="--blink-settings=preferredColorScheme=0"; else flags="--blink-settings=preferredColorScheme=1"; fi
  "$CHROME" --headless=new --disable-gpu --hide-scrollbars --allow-file-access-from-files \
    --window-size=1320,840 --virtual-time-budget=4000 $flags \
    --screenshot="$PWD/shots/$name-$theme.png" "file://$PWD/preview.html?$query" >/dev/null 2>&1
  echo "shots/$name-$theme.png"
done

#!/bin/sh
# Regenerates the app icon asset catalogs from design/icon.svg and design/icon-macos.svg.
# Requires rsvg-convert (brew install librsvg).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
apps="$here/../apps"

ios="$apps/iOS/Assets.xcassets/AppIcon.appiconset"
mac="$apps/Mac/Assets.xcassets/AppIcon.appiconset"
mkdir -p "$ios" "$mac"

for dir in "$apps/iOS/Assets.xcassets" "$apps/Mac/Assets.xcassets"; do
  printf '{\n  "info" : { "author" : "xcode", "version" : 1 }\n}\n' > "$dir/Contents.json"
done

# iOS: one 1024 image; no transparency allowed, so flatten onto the plate colour.
rsvg-convert -w 1024 -h 1024 -b '#07080B' "$here/icon.svg" -o "$ios/icon-1024.png"
cat > "$ios/Contents.json" <<JSON
{
  "images" : [
    { "filename" : "icon-1024.png", "idiom" : "universal", "platform" : "ios", "size" : "1024x1024" }
  ],
  "info" : { "author" : "xcode", "version" : 1 }
}
JSON

# macOS: the classic size ladder.
entries=""
for size in 16 32 128 256 512; do
  for scale in 1 2; do
    px=$((size * scale))
    name="icon-${size}@${scale}x.png"
    rsvg-convert -w "$px" -h "$px" "$here/icon-macos.svg" -o "$mac/$name"
    entries="$entries    { \"filename\" : \"$name\", \"idiom\" : \"mac\", \"scale\" : \"${scale}x\", \"size\" : \"${size}x${size}\" },
"
  done
done
entries=$(printf '%s' "$entries" | sed '$ s/,$//')
cat > "$mac/Contents.json" <<JSON
{
  "images" : [
$entries
  ],
  "info" : { "author" : "xcode", "version" : 1 }
}
JSON

echo "Icons written to $ios and $mac"

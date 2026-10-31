# Turns logo.svg (vtracer output: translated, filled paths) into an Android
# VectorDrawable for the adaptive launcher icon's foreground layer.
import math
import re

# Run from the repository root: python3 droid/icon_from_logo.py
svg_path = "logo.svg"
out_path = "droid/app/src/main/res/drawable/ic_launcher_foreground.xml"
svg = open(svg_path).read()
size = 1024
# Fit the drawing (reaching 495 units from its centre) inside the 66dp safe
# circle of the 108dp icon, so round masks never cut it.
radius = 495.0
viewport = math.ceil(108 * radius / 33)
offset = (viewport - size) / 2
paths = re.findall(r'<path d="([^"]+)" fill="(#[0-9A-Fa-f]{6})" transform="translate\(([-\d.]+),([-\d.]+)\)"/>', svg)
assert len(paths) == svg.count('<path'), 'unexpected path format'
out = [
    '<?xml version="1.0" encoding="utf-8"?>',
    '<!-- Generated from logo.svg (the Flatpak and web icon): the same drawing,',
    '     scaled to fit the adaptive icon\'s safe zone. -->',
    '<vector xmlns:android="http://schemas.android.com/apk/res/android"',
    '    android:width="108dp"',
    '    android:height="108dp"',
    f'    android:viewportWidth="{viewport}"',
    f'    android:viewportHeight="{viewport}">',
    f'    <group android:translateX="{offset:g}" android:translateY="{offset:g}">',
]
for d, fill, x, y in paths:
    out.append(f'        <group android:translateX="{x}" android:translateY="{y}">')
    out.append(f'            <path android:fillColor="{fill.upper()}" android:pathData="{d.strip()}" />')
    out.append('        </group>')
out += ['    </group>', '</vector>', '']
open(out_path, 'w').write('\n'.join(out))
print(len(paths), 'paths, viewport', viewport)

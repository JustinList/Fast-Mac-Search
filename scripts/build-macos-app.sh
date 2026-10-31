#!/usr/bin/env zsh
set -euo pipefail

ROOT_DIR="${0:A:h:h}"
DIST_DIR="$ROOT_DIR/dist"
APP_NAME="Fast Mac Search"
APP_BUNDLE="$DIST_DIR/$APP_NAME.app"
VERSION="0.2.0"
DMG_STAGING="$DIST_DIR/.dmg-staging"
DMG_PATH="$DIST_DIR/Fast-Mac-Search-${VERSION}-arm64.dmg"

echo "==> [1/5] Building optimized Rust engine binaries (searchd + fms)…"
cd "$ROOT_DIR"
cargo build --release

echo "==> [2/5] Building native Swift 6 AppKit UI…"
cd "$ROOT_DIR/clients/macos-ui"
swift build -c release

echo "==> [3/5] Generating Retina AppIcon.icns…"
rm -rf "$DIST_DIR"
mkdir -p "$APP_BUNDLE/Contents/MacOS" \
         "$APP_BUNDLE/Contents/Helpers" \
         "$APP_BUNDLE/Contents/Resources"

ICONSET_DIR="$DIST_DIR/AppIcon.iconset"
mkdir -p "$ICONSET_DIR"

swift - "$ICONSET_DIR" <<'SWIFT_ICON'
import AppKit
import CoreGraphics

let outDir = CommandLine.arguments[1]
let sizes: [(Int, String)] = [
    (16, "icon_16x16.png"),
    (32, "icon_16x16@2x.png"),
    (32, "icon_32x32.png"),
    (64, "icon_32x32@2x.png"),
    (128, "icon_128x128.png"),
    (256, "icon_128x128@2x.png"),
    (256, "icon_256x256.png"),
    (512, "icon_256x256@2x.png"),
    (512, "icon_512x512.png"),
    (1024, "icon_512x512@2x.png")
]

for (px, filename) in sizes {
    let s = CGFloat(px)
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil,
        pixelsWide: px,
        pixelsHigh: px,
        bitsPerSample: 8,
        samplesPerPixel: 4,
        hasAlpha: true,
        isPlanar: false,
        colorSpaceName: .deviceRGB,
        bytesPerRow: 0,
        bitsPerPixel: 0
    )!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let ctx = NSGraphicsContext.current!.cgContext

    let inset = s * 0.08
    let rect = CGRect(x: inset, y: inset, width: s - inset * 2, height: s - inset * 2)
    let radius = rect.width * 0.225
    let bgPath = CGPath(roundedRect: rect, cornerWidth: radius, cornerHeight: radius, transform: nil)

    ctx.saveGState()
    ctx.addPath(bgPath)
    ctx.clip()

    let colors = [
        CGColor(red: 0.07, green: 0.09, blue: 0.16, alpha: 1.0),
        CGColor(red: 0.12, green: 0.17, blue: 0.30, alpha: 1.0)
    ] as CFArray
    let space = CGColorSpaceCreateDeviceRGB()
    let grad = CGGradient(colorsSpace: space, colors: colors, locations: [0.0, 1.0])!
    ctx.drawLinearGradient(
        grad,
        start: CGPoint(x: rect.minX, y: rect.minY),
        end: CGPoint(x: rect.maxX, y: rect.maxY),
        options: []
    )

    // Subtle glowing search ring
    ctx.setStrokeColor(CGColor(red: 0.30, green: 0.68, blue: 1.0, alpha: 0.32))
    ctx.setLineWidth(s * 0.035)
    let ringR = rect.width * 0.31
    ctx.strokeEllipse(in: CGRect(x: s * 0.5 - ringR, y: s * 0.5 - ringR, width: ringR * 2, height: ringR * 2))

    // Electric lightning bolt
    let bolt = CGMutablePath()
    bolt.move(to: CGPoint(x: s * 0.55, y: s * 0.77))
    bolt.addLine(to: CGPoint(x: s * 0.34, y: s * 0.48))
    bolt.addLine(to: CGPoint(x: s * 0.49, y: s * 0.48))
    bolt.addLine(to: CGPoint(x: s * 0.44, y: s * 0.23))
    bolt.addLine(to: CGPoint(x: s * 0.66, y: s * 0.52))
    bolt.addLine(to: CGPoint(x: s * 0.51, y: s * 0.52))
    bolt.closeSubpath()

    ctx.setFillColor(CGColor(red: 1.0, green: 0.78, blue: 0.22, alpha: 1.0))
    ctx.addPath(bolt)
    ctx.fillPath()
    ctx.restoreGState()

    NSGraphicsContext.restoreGraphicsState()
    if let png = rep.representation(using: .png, properties: [:]) {
        try! png.write(to: URL(fileURLWithPath: "\(outDir)/\(filename)"))
    }
}
SWIFT_ICON

iconutil -c icns "$ICONSET_DIR" -o "$APP_BUNDLE/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET_DIR"

echo "==> [4/5] Assembling '$APP_NAME.app' bundle & signing…"
cp "$ROOT_DIR/clients/macos-ui/.build/release/FastMacSearchUI" "$APP_BUNDLE/Contents/MacOS/FastMacSearch"
cp "$ROOT_DIR/target/release/searchd" "$APP_BUNDLE/Contents/Helpers/searchd"
cp "$ROOT_DIR/target/release/fms" "$APP_BUNDLE/Contents/Helpers/fms"
chmod +x "$APP_BUNDLE/Contents/MacOS/FastMacSearch" \
         "$APP_BUNDLE/Contents/Helpers/searchd" \
         "$APP_BUNDLE/Contents/Helpers/fms"

printf "APPL????" > "$APP_BUNDLE/Contents/PkgInfo"

cat > "$APP_BUNDLE/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>Fast Mac Search</string>
    <key>CFBundleExecutable</key>
    <string>FastMacSearch</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>dev.fastmacsearch.app</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Fast Mac Search</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.2.0</string>
    <key>CFBundleVersion</key>
    <string>2</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

# Strip external-volume AppleDouble / resource-fork attributes before codesign
find "$APP_BUNDLE" -name '._*' -delete 2>/dev/null || true
xattr -cr "$APP_BUNDLE"
codesign --force --deep --sign - "$APP_BUNDLE"

echo "==> [5/5] Creating drag-to-Applications disk image ($DMG_PATH)…"
mkdir -p "$DMG_STAGING"
cp -R "$APP_BUNDLE" "$DMG_STAGING/"
ln -s /Applications "$DMG_STAGING/Applications"
find "$DMG_STAGING" -name '._*' -delete 2>/dev/null || true

hdiutil create \
    -volname "Fast Mac Search" \
    -srcfolder "$DMG_STAGING" \
    -ov \
    -format UDZO \
    "$DMG_PATH" >/dev/null

rm -rf "$DMG_STAGING"

echo ""
echo "✅ Built macOS Application & Installer DMG:"
echo "   App Bundle : $APP_BUNDLE"
echo "   Disk Image : $DMG_PATH"
